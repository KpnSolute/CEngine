//! The audit tool: every write is recorded with who, what and before/after, in the same
//! transaction, and nobody can write the trail through the API.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, AppState};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "aud-1";

fn token(tenant: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{tenant}","roles":["owner"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn url() -> Option<String> {
    match std::env::var("CE_TEST_DATABASE_URL") {
        Ok(u) => Some(u),
        Err(_) => {
            assert!(
                std::env::var("CE_REQUIRE_DB").is_err(),
                "CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not"
            );
            eprintln!("skipping: CE_TEST_DATABASE_URL not set");
            None
        }
    }
}

static N: AtomicU32 = AtomicU32::new(0);

fn build(url: &str, m: Value) -> Router {
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_aud_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

async fn app(url: &str, m: Value) -> Router {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || build(&url, m))
        .await
        .unwrap()
}

async fn close(a: Router) {
    tokio::task::spawn_blocking(move || drop(a)).await.unwrap();
}

struct Api {
    app: Router,
    n: u32,
}

impl Api {
    async fn call(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.n += 1;
        let key = format!("k{}", self.n);
        self.call_keyed(method, uri, version, body, &key).await
    }

    async fn call_keyed(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
        key: &str,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", token(T1)))
            .header("idempotency-key", key);
        if let Some(v) = version {
            req = req.header("if-match", format!("\"{v}\""));
        }
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let res = self
            .app
            .clone()
            .oneshot(
                req.body(match body {
                    Some(b) => Body::from(b.to_string()),
                    None => Body::empty(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn post(&mut self, entity: &str, body: Value) -> (StatusCode, Value) {
        self.call("POST", &format!("/v1/{INST}/{entity}"), None, Some(body))
            .await
    }
}

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"widget": {"fields": {"name": {"type": "string", "required": true}}}},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        }),
        &["audit"],
    )
    .unwrap()
}

#[tokio::test]
async fn every_write_is_recorded_with_who_and_before_after() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest()).await,
        n: 0,
    };
    let (s, w) = api.post("widget", json!({"name": "a"})).await;
    assert_eq!(s, StatusCode::CREATED, "{w}");
    let id = w["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = api
        .call(
            "PATCH",
            &format!("/v1/{INST}/widget/{id}"),
            Some(1),
            Some(json!({"name": "b"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = api
        .call("DELETE", &format!("/v1/{INST}/widget/{id}"), Some(2), None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");

    let (s, v) = api
        .call(
            "GET",
            &format!("/v1/{INST}/audit_event?sort=at"),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{v}");
    let mut ops: Vec<&str> = rows
        .iter()
        .map(|r| r["operation"].as_str().unwrap())
        .collect();
    ops.sort();
    assert_eq!(ops, ["create", "delete", "update"]);
    for r in rows {
        assert_eq!(r["actor"], "u1");
        assert_eq!(r["entity"], "widget");
        assert_eq!(r["record_id"], id.as_str());
        assert!(r["request_id"].as_str().is_some_and(|s| !s.is_empty()));
    }
    let upd = rows.iter().find(|r| r["operation"] == "update").unwrap();
    assert_eq!(upd["before"]["name"], "a");
    assert_eq!(upd["after"]["name"], "b");
    assert_eq!(upd["to_version"], 2);
    let del = rows.iter().find(|r| r["operation"] == "delete").unwrap();
    assert_eq!(del["before"]["name"], "b");
    close(api.app).await;
}

#[tokio::test]
async fn a_replayed_create_is_not_audited_twice_and_a_failed_write_is_not_audited() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest()).await,
        n: 0,
    };
    let body = json!({"name": "a"});
    let (s1, _) = api
        .call_keyed(
            "POST",
            &format!("/v1/{INST}/widget"),
            None,
            Some(body.clone()),
            "same",
        )
        .await;
    let (s2, _) = api
        .call_keyed(
            "POST",
            &format!("/v1/{INST}/widget"),
            None,
            Some(body),
            "same",
        )
        .await;
    assert_eq!((s1, s2), (StatusCode::CREATED, StatusCode::CREATED));
    // A write that fails validation leaves no trail either.
    let (s, _) = api.post("widget", json!({})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, v) = api
        .call("GET", &format!("/v1/{INST}/audit_event"), None, None)
        .await;
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "{v}");
    close(api.app).await;
}

#[tokio::test]
async fn the_trail_cannot_be_written_through_the_api() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest()).await,
        n: 0,
    };
    let (s, v) = api
        .post(
            "audit_event",
            json!({"entity": "widget", "record_id": "x", "operation": "create", "actor": "me", "at": 1}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    close(api.app).await;
}
