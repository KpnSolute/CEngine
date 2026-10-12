//! Periods: closing one freezes the records that point at it; reopening thaws them.
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
const INST: &str = "pd-1";

fn token(tenant: &str, role: &str, sub: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{sub}","tenant":"{tenant}","roles":["{role}"],"exp":2000}}"#
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
        "ce_pd_{}_{}",
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

/// Drops every handle to the app away from the async runtime: closing the database client
/// blocks, which is not allowed on a runtime thread.
async fn close(apps: Vec<Router>) {
    tokio::task::spawn_blocking(move || drop(apps))
        .await
        .unwrap();
}

struct Api {
    app: Router,
    n: u32,
    role: &'static str,
    extra: Option<String>,
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
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(T1, self.role, self.role)),
            )
            .header("idempotency-key", key);
        if let Some(c) = &self.extra {
            req = req.header("x-ce-changeset", c.as_str());
        }
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
    async fn verb(&mut self, id: &str, verb: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/period/{id}/{verb}"),
            None,
            None,
        )
        .await
    }
    async fn list(&mut self, entity: &str) -> Vec<Value> {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/{entity}?limit=100"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].as_array().unwrap().clone()
    }
}

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"sale": {
                "fields": {
                    "item": {"type": "string", "required": true, "indexed": true},
                    "period": {"type": "ref", "ref": "period", "required": true, "indexed": true}
                },
                "locked_when": "ref(period).status == 'closed'"
            }},
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "clerk": {"read": ["*"], "write": ["sale"], "actions": []},
                "closer": {"read": ["*"], "write": ["period"], "actions": ["close_period"]}
            }
        }),
        &["audit", "period"],
    )
    .unwrap()
}

async fn api(role: &'static str, app: Router) -> Api {
    Api {
        app,
        n: 0,
        role,
        extra: None,
    }
}

fn as_role(a: &Api, role: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        n: 1000,
        role,
        extra: None,
    }
}

async fn setup() -> Option<Api> {
    let url = url()?;
    Some(api("clerk", app(&url, manifest()).await).await)
}

fn id(v: &Value) -> String {
    v["data"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn closing_a_period_freezes_its_records_and_reopening_thaws_them() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut closer = as_role(&clerk, "closer");
    let (s, p) = closer
        .post(
            "period",
            json!({"label": "2026-09", "starts_on": "2026-09-01", "ends_on": "2026-09-30"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["data"]["status"], "open");
    let pid = id(&p);
    let (s, sale) = clerk
        .post("sale", json!({"item": "soup", "period": pid}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{sale}");
    let sid = id(&sale);

    // A clerk cannot close; the closer can, and the snapshot counts the records.
    let (s, _) = clerk.verb(&pid, "close").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = closer.verb(&pid, "close").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "closed");
    assert_eq!(v["data"]["closed_by"], "closer");
    assert_eq!(v["data"]["snapshot"]["counts"]["sale"], 1);
    let (s, _) = closer.verb(&pid, "close").await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Frozen: no new record, no edit, no delete in the closed period.
    let (s, v) = clerk
        .post("sale", json!({"item": "bread", "period": pid}))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "entity_locked");
    let (s, v) = clerk
        .call(
            "PATCH",
            &format!("/v1/{INST}/sale/{sid}"),
            Some(1),
            Some(json!({"item": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, _) = clerk
        .call("DELETE", &format!("/v1/{INST}/sale/{sid}"), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Another period is unaffected.
    let (_, p2) = closer
        .post(
            "period",
            json!({"label": "2026-10", "starts_on": "2026-10-01", "ends_on": "2026-10-31"}),
        )
        .await;
    let (s, v) = clerk
        .post("sale", json!({"item": "bread", "period": id(&p2)}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // A record cannot be moved into the closed period either.
    let (s, _) = clerk
        .call(
            "PATCH",
            &format!("/v1/{INST}/sale/{}", id(&v)),
            Some(1),
            Some(json!({"period": pid})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Reopened, the first period takes edits again.
    let (s, v) = closer.verb(&pid, "reopen").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "open");
    let (s, _) = closer.verb(&pid, "reopen").await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v) = clerk
        .call(
            "PATCH",
            &format!("/v1/{INST}/sale/{sid}"),
            Some(1),
            Some(json!({"item": "x"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    close(vec![clerk.app, closer.app]).await;
}

#[tokio::test]
async fn only_the_close_endpoint_sets_the_status() {
    let Some(clerk) = setup().await else { return };
    let mut closer = as_role(&clerk, "closer");
    let (s, v) = closer
        .post(
            "period",
            json!({"label": "p", "starts_on": "2026-01-01", "ends_on": "2026-01-31", "status": "closed"}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (_, p) = closer
        .post(
            "period",
            json!({"label": "p", "starts_on": "2026-01-01", "ends_on": "2026-01-31"}),
        )
        .await;
    let (s, _) = closer
        .call(
            "PATCH",
            &format!("/v1/{INST}/period/{}", id(&p)),
            Some(1),
            Some(json!({"status": "closed"})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Closing and reopening leave audit rows.
    let pid = id(&p);
    closer.verb(&pid, "close").await;
    closer.verb(&pid, "reopen").await;
    let mut owner = as_role(&closer, "owner");
    let audit = owner.list("audit_event").await;
    assert_eq!(audit.len(), 3, "{audit:?}");
    close(vec![closer.app, owner.app, clerk.app]).await;
}
