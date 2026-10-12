//! Changesets: governed writes by callers who cannot merge are staged, then merged, rejected
//! or reverted by a reviewer; a merge is all-or-nothing.
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
const INST: &str = "cs-1";

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
        "ce_cs_{}_{}",
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

    async fn call_with(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
        changeset: &str,
    ) -> (StatusCode, Value) {
        self.extra = Some(changeset.to_string());
        let r = self.call(method, uri, version, body).await;
        self.extra = None;
        r
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
            &format!("/v1/{INST}/changeset/{id}/{verb}"),
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
            "entities": {"widget": {
                "fields": {
                    "name": {"type": "string", "required": true, "indexed": true},
                    "qty": {"type": "integer"}
                },
                "governed": true
            }},
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "clerk": {"read": ["*"], "write": ["widget"], "actions": []},
                "reviewer": {"read": ["*"], "write": [], "actions": ["merge_changeset"]}
            }
        }),
        &["audit", "changesets"],
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

/// Same app, another caller.
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

#[tokio::test]
async fn a_clerks_write_waits_until_a_reviewer_merges_it() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut rev = as_role(&clerk, "reviewer");
    let (s, v) = clerk.post("widget", json!({"name": "a", "qty": 1})).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    assert!(clerk.list("widget").await.is_empty(), "not applied yet");
    // A clerk cannot merge.
    let (s, _) = clerk.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "merged");
    let widgets = rev.list("widget").await;
    assert_eq!(widgets.len(), 1);
    assert_eq!(widgets[0]["name"], "a");
    // The audit trail names the reviewer, who applied it.
    let audit = rev.list("audit_event").await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["actor"], "reviewer");
    // A merged changeset cannot merge twice.
    let (s, _) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::CONFLICT);
    close(vec![clerk.app, rev.app]).await;
}

#[tokio::test]
async fn an_owner_writes_directly_and_internal_records_are_not_writable() {
    let Some(clerk) = setup().await else { return };
    let mut owner = as_role(&clerk, "owner");
    let (s, v) = owner.post("widget", json!({"name": "direct"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _) = owner
        .post("changeset", json!({"title": "x", "submitted_by": "me"}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    close(vec![clerk.app, owner.app]).await;
}

#[tokio::test]
async fn an_update_merges_then_reverts() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut owner = as_role(&clerk, "owner");
    let mut rev = as_role(&clerk, "reviewer");
    let (_, w) = owner.post("widget", json!({"name": "a", "qty": 1})).await;
    let id = w["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = clerk
        .call(
            "PATCH",
            &format!("/v1/{INST}/widget/{id}"),
            Some(1),
            Some(json!({"qty": 5})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    assert_eq!(rev.list("widget").await[0]["qty"], 1);
    let (s, v) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(rev.list("widget").await[0]["qty"], 5);
    let (s, v) = rev.verb(&cs, "revert").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "reverted");
    assert_eq!(rev.list("widget").await[0]["qty"], 1);
    close(vec![clerk.app, owner.app, rev.app]).await;
}

#[tokio::test]
async fn a_changeset_that_no_longer_applies_is_marked_conflicted() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut owner = as_role(&clerk, "owner");
    let mut rev = as_role(&clerk, "reviewer");
    let (_, w) = owner.post("widget", json!({"name": "a", "qty": 1})).await;
    let id = w["data"]["id"].as_str().unwrap().to_string();
    let (_, v) = clerk
        .call(
            "PATCH",
            &format!("/v1/{INST}/widget/{id}"),
            Some(1),
            Some(json!({"qty": 5})),
        )
        .await;
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    // The owner changes the record first.
    let (s, _) = owner
        .call(
            "PATCH",
            &format!("/v1/{INST}/widget/{id}"),
            Some(1),
            Some(json!({"qty": 9})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "version_conflict");
    let sets = rev.list("changeset").await;
    assert_eq!(sets[0]["status"], "conflicted");
    assert_eq!(rev.list("widget").await[0]["qty"], 9);
    close(vec![clerk.app, owner.app, rev.app]).await;
}

#[tokio::test]
async fn a_merge_is_all_or_nothing_and_a_create_can_be_reverted() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut owner = as_role(&clerk, "owner");
    let mut rev = as_role(&clerk, "reviewer");
    let (_, w) = owner.post("widget", json!({"name": "a", "qty": 1})).await;
    let id = w["data"]["id"].as_str().unwrap().to_string();
    // Two operations in one changeset: a create, then an update with a stale version.
    let (_, v) = clerk.post("widget", json!({"name": "new"})).await;
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, _) = clerk
        .call_with(
            "PATCH",
            &format!("/v1/{INST}/widget/{id}"),
            Some(7),
            Some(json!({"qty": 2})),
            &cs,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, _) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(rev.list("widget").await.len(), 1, "the create was not kept");

    // A good changeset with a create reverts to nothing.
    let (_, v) = clerk.post("widget", json!({"name": "temp"})).await;
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, _) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(rev.list("widget").await.len(), 2);
    let (s, _) = rev.verb(&cs, "revert").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(rev.list("widget").await.len(), 1);
    close(vec![clerk.app, owner.app, rev.app]).await;
}

#[tokio::test]
async fn reject_withdraw_and_deletes() {
    let Some(mut clerk) = setup().await else {
        return;
    };
    let mut owner = as_role(&clerk, "owner");
    let mut rev = as_role(&clerk, "reviewer");
    let (_, v) = clerk.post("widget", json!({"name": "a"})).await;
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, v) = rev.verb(&cs, "reject").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "rejected");
    let (_, v) = clerk.post("widget", json!({"name": "b"})).await;
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    // Only the submitter withdraws.
    let (s, _) = rev.verb(&cs, "withdraw").await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, v) = clerk.verb(&cs, "withdraw").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(owner.list("widget").await.is_empty());

    // A staged delete merges, and cannot be reverted.
    let (_, w) = owner.post("widget", json!({"name": "gone"})).await;
    let id = w["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = clerk
        .call("DELETE", &format!("/v1/{INST}/widget/{id}"), Some(1), None)
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, mv) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::OK, "{mv}");
    assert!(owner.list("widget").await.is_empty());
    let (s, v) = rev.verb(&cs, "revert").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    close(vec![clerk.app, owner.app, rev.app]).await;
}
