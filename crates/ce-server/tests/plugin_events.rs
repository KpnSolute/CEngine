//! A plugin reacts to an engine event: a merged changeset stores `changeset.merged`, and the
//! plugin's subscription creates a record in the plugin's own entity in the same transaction.
//! A reaction the rules refuse is recorded on the event and never undoes the merge.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate_composed;
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
const INST: &str = "pev-1";
const DIGEST: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn token(role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{role}","tenant":"{T1}","roles":["{role}"],"exp":2000}}"#
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
    let m = validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_pev_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

struct Api {
    app: Router,
    n: u32,
    role: &'static str,
    changeset: Option<String>,
}

impl Api {
    async fn call(&mut self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.n += 1;
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(self.role)),
            )
            .header("idempotency-key", format!("{}-{}", self.role, self.n));
        if let Some(c) = &self.changeset {
            req = req.header("x-ce-changeset", c.as_str());
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
            .call("GET", &format!("/v1/{INST}/{entity}?limit=100"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].as_array().unwrap().clone()
    }
}

const PLUGIN: &str = r#"schema_version = 1
[plugin]
id = "archiver"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"

[[entity]]
name = "job"
[entity.definition.fields.changeset]
type = "string"
required = true
max_length = 80
[entity.definition.fields.actor]
type = "string"
max_length = 80
[entity.definition.fields.state]
type = "string"
max_length = 20

[[entity]]
name = "strict"
[entity.definition.fields.needed]
type = "string"
required = true
max_length = 40
[entity.definition.fields.actor]
type = "string"
max_length = 80

[[subscribe]]
event = "changeset.merged"
entity = "job"
[subscribe.values.changeset]
from = "subject"
[subscribe.values.actor]
from = "actor"
[subscribe.values.state]
value = "queued"

[[subscribe]]
event = "changeset.merged"
entity = "strict"
[subscribe.values.actor]
from = "actor"
"#;

fn manifest() -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {
            "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
            "clerk": {"read": ["*"], "write": ["stock_item"], "actions": []},
            "reviewer": {"read": ["*"], "write": [], "actions": ["merge_changeset", "manage_events"]}
        }
    });
    let shell = ce_tools::compose(&shell, &["period", "audit", "changesets", "events"]).unwrap();
    let plugin = ce_plugin::parse(
        PLUGIN,
        &ce_plugin::builtin_registry(),
        &semver::Version::new(0, 1, 0),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let mut m = ce_plugin::compose(&shell, "business.inventory", Some((&plugin, DIGEST)))
        .unwrap_or_else(|e| panic!("{e:?}"))
        .manifest;
    m["entities"]["stock_item"]["governed"] = json!(true);
    m
}

#[tokio::test]
async fn a_merge_stores_an_event_and_the_plugin_reacts_in_the_same_transaction() {
    let Some(url) = url() else { return };
    let m = manifest();
    let app = tokio::task::spawn_blocking(move || build(&url, m))
        .await
        .unwrap();
    let mut clerk = Api {
        app: app.clone(),
        n: 0,
        role: "clerk",
        changeset: None,
    };
    let mut reviewer = Api {
        app: app.clone(),
        n: 0,
        role: "reviewer",
        changeset: None,
    };

    // Nothing has merged yet: no event, no job.
    assert!(reviewer.list("archiver__job").await.is_empty());

    let (s, v) = clerk
        .call(
            "POST",
            &format!("/v1/{INST}/stock_item"),
            Some(json!({"sku": "RICE", "description": "Rice"})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    assert!(reviewer.list("archiver__job").await.is_empty());

    let (s, v) = reviewer
        .call("POST", &format!("/v1/{INST}/changeset/{cs}/merge"), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // The merge went through, and the plugin's job exists, copied from the event.
    let jobs = reviewer.list("archiver__job").await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0]["changeset"], cs.as_str());
    assert_eq!(jobs[0]["actor"], "reviewer");
    assert_eq!(jobs[0]["state"], "queued");
    assert_eq!(reviewer.list("stock_item").await.len(), 1);

    // The stored event is `changeset.merged` for that changeset, and it names the reaction the
    // rules refused (the `strict` entity needs a field the subscription does not give).
    let events = reviewer.list("outbox_event").await;
    let merged: Vec<&Value> = events
        .iter()
        .filter(|e| e["name"] == "changeset.merged")
        .collect();
    assert_eq!(merged.len(), 1, "{events:?}");
    assert_eq!(merged[0]["subject"], cs.as_str());
    let refused = &merged[0]["data"]["reaction_errors"];
    assert_eq!(refused.as_array().map(Vec::len), Some(1), "{refused}");
    assert_eq!(refused[0]["entity"], "archiver__strict");
    assert!(reviewer.list("archiver__strict").await.is_empty());

    tokio::task::spawn_blocking(move || drop((app, clerk, reviewer)))
        .await
        .unwrap();
}
