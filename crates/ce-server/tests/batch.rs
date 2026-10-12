//! Batch writes: many operations in one transaction, all or nothing.
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
const INST: &str = "bt-1";

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
        "ce_bt_{}_{}",
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
            "entities": {
                "widget": {"fields": {
                    "name": {"type": "string", "required": true, "unique": true},
                    "qty": {"type": "integer"}
                }},
                "secret": {"fields": {"v": {"type": "string"}}}
            },
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "clerk": {"read": ["*"], "write": ["widget"], "actions": []},
                "dropbox": {"read": [], "write": ["widget"], "actions": []}
            }
        }),
        &["audit"],
    )
    .unwrap()
}

async fn setup() -> Option<Api> {
    let url = url()?;
    Some(Api {
        app: app(&url, manifest()).await,
        n: 0,
        role: "owner",
        extra: None,
    })
}

impl Api {
    async fn batch(&mut self, ops: Value) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/_batch"),
            None,
            Some(json!({"ops": ops})),
        )
        .await
    }
}

#[tokio::test]
async fn a_batch_applies_every_operation_in_order() {
    let Some(mut api) = setup().await else {
        return;
    };
    let (s, v) = api
        .batch(json!([
            {"op": "create", "entity": "widget", "data": {"name": "a", "qty": 1}},
            {"op": "create", "entity": "widget", "data": {"name": "b", "qty": 2}},
        ]))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
    let a = v["data"][0]["id"].as_str().unwrap().to_string();
    let b = v["data"][1]["id"].as_str().unwrap().to_string();
    let (s, v) = api
        .batch(json!([
            {"op": "update", "entity": "widget", "id": a, "version": 1, "data": {"qty": 10}},
            {"op": "delete", "entity": "widget", "id": b, "version": 1},
        ]))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"][0]["qty"], 10);
    assert!(v["data"][1].is_null());
    let left = api.list("widget").await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["qty"], 10);
    assert_eq!(api.list("audit_event").await.len(), 4);
    close(vec![api.app]).await;
}

#[tokio::test]
async fn one_failing_operation_undoes_the_whole_batch() {
    let Some(mut api) = setup().await else {
        return;
    };
    let (s, v) = api
        .batch(json!([
            {"op": "create", "entity": "widget", "data": {"name": "a"}},
            {"op": "update", "entity": "widget", "id": "00000000-0000-0000-0000-000000000000", "version": 1, "data": {"qty": 1}},
        ]))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert!(api.list("widget").await.is_empty());
    assert!(api.list("audit_event").await.is_empty());
    // A duplicate inside the batch fails the whole batch too.
    let (s, _) = api
        .batch(json!([
            {"op": "create", "entity": "widget", "data": {"name": "x"}},
            {"op": "create", "entity": "widget", "data": {"name": "x"}},
        ]))
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(api.list("widget").await.is_empty());
    close(vec![api.app]).await;
}

#[tokio::test]
async fn a_batch_is_checked_before_anything_is_written() {
    let Some(mut owner) = setup().await else {
        return;
    };
    let mut clerk = Api {
        app: owner.app.clone(),
        n: 1000,
        role: "clerk",
        extra: None,
    };
    // The clerk may not write secrets: the whole batch is refused.
    let (s, _) = clerk
        .batch(json!([
            {"op": "create", "entity": "widget", "data": {"name": "a"}},
            {"op": "create", "entity": "secret", "data": {"v": "x"}},
        ]))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Malformed operations and server-only records are refused.
    for ops in [
        json!([]),
        json!([{"op": "create", "entity": "widget"}]),
        json!([{"op": "update", "entity": "widget", "id": "x", "data": {}}]),
        json!([{"op": "nope", "entity": "widget"}]),
        json!([{"op": "create", "entity": "nothing", "data": {}}]),
    ] {
        let (s, v) = clerk.batch(ops).await;
        assert!(
            s == StatusCode::UNPROCESSABLE_ENTITY || s == StatusCode::FORBIDDEN,
            "{s} {v}"
        );
    }
    let (s, _) = owner
        .batch(json!([{"op": "create", "entity": "audit_event", "data": {"entity": "x"}}]))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(owner.list("widget").await.is_empty());
    close(vec![owner.app, clerk.app]).await;
}

#[tokio::test]
async fn a_caller_without_read_access_gets_only_id_and_version() {
    let Some(owner) = setup().await else { return };
    let mut drop = Api {
        app: owner.app.clone(),
        n: 1000,
        role: "dropbox",
        extra: None,
    };
    let (s, v) = drop
        .batch(json!([{"op": "create", "entity": "widget", "data": {"name": "a", "qty": 5}}]))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let r = v["data"][0].as_object().unwrap();
    assert!(r.contains_key("id") && r.contains_key("version"));
    assert!(!r.contains_key("name") && !r.contains_key("qty"), "{r:?}");
    close(vec![owner.app, drop.app]).await;
}

#[tokio::test]
async fn batches_that_touch_the_same_records_in_opposite_orders_both_finish() {
    let Some(mut api) = setup().await else {
        return;
    };
    let (_, v) = api
        .batch(json!([
            {"op": "create", "entity": "widget", "data": {"name": "a"}},
            {"op": "create", "entity": "widget", "data": {"name": "b"}},
        ]))
        .await;
    let a = v["data"][0]["id"].as_str().unwrap().to_string();
    let b = v["data"][1]["id"].as_str().unwrap().to_string();
    let mut one = Api {
        app: api.app.clone(),
        n: 2000,
        role: "owner",
        extra: None,
    };
    let mut two = Api {
        app: api.app.clone(),
        n: 3000,
        role: "owner",
        extra: None,
    };
    let (x, y) = tokio::join!(
        async {
            let mut codes = Vec::new();
            for i in 0..5 {
                let (s, _) = one
                    .batch(json!([
                        {"op": "update", "entity": "widget", "id": a, "version": 1 + 2 * i, "data": {"qty": i}},
                        {"op": "update", "entity": "widget", "id": b, "version": 1 + 2 * i, "data": {"qty": i}},
                    ]))
                    .await;
                codes.push(s);
            }
            codes
        },
        async {
            let mut codes = Vec::new();
            for i in 0..5 {
                let (s, _) = two
                    .batch(json!([
                        {"op": "update", "entity": "widget", "id": b, "version": 1 + 2 * i, "data": {"qty": 9}},
                        {"op": "update", "entity": "widget", "id": a, "version": 1 + 2 * i, "data": {"qty": 9}},
                    ]))
                    .await;
                codes.push(s);
            }
            codes
        }
    );
    // Versions may clash (409) but no request may fail with a server error from a deadlock.
    for s in x.into_iter().chain(y) {
        assert!(s == StatusCode::OK || s == StatusCode::CONFLICT, "{s}");
    }
    close(vec![api.app, one.app, two.app]).await;
}
