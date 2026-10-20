//! A plugin's queued record is delivered through the host's connector: retried with backoff on a
//! failure, marked done with the connector's answer, and dead after too many failures.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate_composed;
use ce_provider::{Provider, ProviderError, ProviderRequest, ProviderResponse};
use ce_server::{router, AppState};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "pdl-1";
const DIGEST: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

fn token(role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{role}","tenant":"{T1}","roles":["{role}"],"exp":99999999}}"#
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

fn build(url: &str, m: Value, clock: Arc<AtomicU64>, provider: Arc<Fake>) -> Router {
    let m = validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_pdl_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = Arc::new(move || clock.load(Ordering::SeqCst));
    st.provider = Some(provider);
    router(st)
}

/// The store closes its connections on drop, which blocks on its own runtime. Dropped on a
/// runtime thread (as happens when an assertion panics) that aborts the process and hides every
/// other result, so the drop always runs on a thread of its own.
#[derive(Clone)]
struct Off(Option<Router>);

impl std::ops::Deref for Off {
    type Target = Router;
    fn deref(&self) -> &Router {
        self.0.as_ref().unwrap()
    }
}

impl Drop for Off {
    fn drop(&mut self) {
        if let Some(app) = self.0.take() {
            let _ = std::thread::spawn(move || drop(app)).join();
        }
    }
}

struct Api {
    app: Off,
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
            .0
            .clone()
            .unwrap()
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
    /// Every record, following the page cursor: one page of an unsorted list is not the set.
    async fn list(&mut self, entity: &str) -> Vec<Value> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut uri = format!("/v1/{INST}/{entity}?limit=100");
            if let Some(a) = &after {
                uri.push_str(&format!("&after={a}"));
            }
            let (s, v) = self.call("GET", &uri, None).await;
            assert_eq!(s, StatusCode::OK, "{v}");
            out.extend(v["data"].as_array().unwrap().iter().cloned());
            match v["meta"]["next"].as_str() {
                Some(n) => after = Some(n.to_string()),
                None => return out,
            }
        }
    }
}

/// A connector that answers from a script and records what it was asked.
struct Fake {
    calls: Mutex<Vec<ProviderRequest>>,
    fail: Mutex<Option<ProviderError>>,
}

impl Provider for Fake {
    fn call(
        &self,
        _scope: &ce_auth::TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.calls.lock().unwrap().push(req.clone());
        match self.fail.lock().unwrap().clone() {
            Some(e) => Err(e),
            None => Ok(ProviderResponse {
                data: json!({"sha": "abc123"}),
            }),
        }
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
[entity.definition.fields.status]
type = "string"
max_length = 20
[entity.definition.fields.attempts]
type = "integer"
[entity.definition.fields.not_before]
type = "integer"
[entity.definition.fields.last_error]
type = "string"
max_length = 400
[entity.definition.fields.result]
type = "json"

[[subscribe]]
event = "changeset.merged"
entity = "job"
[subscribe.values.changeset]
from = "subject"
[subscribe.values.status]
value = "queued"

[[deliver]]
entity = "job"
connector = "storage"
operation = "archive.put"
[deliver.params.changeset]
from = "changeset"
[deliver.params.kind]
value = "snapshot"
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

struct Rig {
    clerk: Api,
    reviewer: Api,
    clock: Arc<AtomicU64>,
    fake: Arc<Fake>,
}

async fn rig() -> Option<Rig> {
    let url = url()?;
    let (clock, fake) = (
        Arc::new(AtomicU64::new(1000)),
        Arc::new(Fake {
            calls: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
        }),
    );
    let (m, c, f) = (manifest(), clock.clone(), fake.clone());
    let app = tokio::task::spawn_blocking(move || build(&url, m, c, f))
        .await
        .unwrap();
    let mk = |role| Api {
        app: Off(Some(app.clone())),
        n: 0,
        role,
        changeset: None,
    };
    let (mut clerk, mut reviewer) = (mk("clerk"), mk("reviewer"));
    let (s, v) = clerk
        .call(
            "POST",
            &format!("/v1/{INST}/stock_item"),
            Some(json!({"sku": "RICE", "description": "Rice"})),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, v) = reviewer
        .call("POST", &format!("/v1/{INST}/changeset/{cs}/merge"), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    Some(Rig {
        clerk,
        reviewer,
        clock,
        fake,
    })
}

impl Rig {
    /// The store closes its connections on drop, which blocks, so drop it off the runtime.
    async fn finish(self) {
        tokio::task::spawn_blocking(move || drop(self))
            .await
            .unwrap();
    }
    async fn run(&mut self) -> Value {
        let (s, v) = self
            .reviewer
            .call("POST", &format!("/v1/{INST}/deliveries/run"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].clone()
    }
    async fn job(&mut self) -> Value {
        let jobs = self.reviewer.list("archiver__job").await;
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        jobs[0].clone()
    }
}

#[tokio::test]
async fn a_queued_job_is_retried_with_backoff_then_delivered() {
    let Some(mut r) = rig().await else { return };
    assert_eq!(r.job().await["status"], "queued");

    // The connector is down: the job waits, and a second pass before the backoff does nothing.
    *r.fake.fail.lock().unwrap() = Some(ProviderError::Unavailable);
    assert_eq!(
        r.run().await,
        json!({"delivered": 0, "retrying": 1, "dead": 0})
    );
    let j = r.job().await;
    assert_eq!(
        (&j["status"], &j["attempts"]),
        (&json!("queued"), &json!(1))
    );
    assert_eq!(j["not_before"], 1005);
    assert_eq!(j["last_error"], "the connector is unavailable");
    assert_eq!(
        r.run().await,
        json!({"delivered": 0, "retrying": 0, "dead": 0})
    );
    assert_eq!(r.fake.calls.lock().unwrap().len(), 1);

    // Time passes and the connector is back: delivered, with its answer kept.
    *r.fake.fail.lock().unwrap() = None;
    r.clock.store(2000, Ordering::SeqCst);
    assert_eq!(
        r.run().await,
        json!({"delivered": 1, "retrying": 0, "dead": 0})
    );
    let j = r.job().await;
    assert_eq!((&j["status"], &j["attempts"]), (&json!("done"), &json!(2)));
    assert_eq!(j["result"], json!({"sha": "abc123"}));
    assert!(j["last_error"].is_null());

    // The call carried the configured operation and params, and one idempotency key for the
    // job across both attempts. A done job is not sent again.
    let calls = r.fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].connector, ce_provider::ConnectorType::Storage);
    assert_eq!(calls[1].operation, "archive.put");
    assert_eq!(calls[1].params["kind"], "snapshot");
    assert_eq!(calls[1].params["changeset"], j["changeset"]);
    let key = format!("archiver__job:{}", j["id"].as_str().unwrap());
    assert_eq!(calls[0].idempotency_key.as_deref(), Some(key.as_str()));
    assert_eq!(calls[1].idempotency_key.as_deref(), Some(key.as_str()));
    assert_eq!(
        r.run().await,
        json!({"delivered": 0, "retrying": 0, "dead": 0})
    );
    assert_eq!(r.fake.calls.lock().unwrap().len(), 2);

    // Only a role that manages events may run deliveries.
    let (s, _) = r
        .clerk
        .call("POST", &format!("/v1/{INST}/deliveries/run"), None)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    r.finish().await;
}

#[tokio::test]
async fn a_job_that_keeps_failing_ends_dead() {
    let Some(mut r) = rig().await else { return };
    *r.fake.fail.lock().unwrap() = Some(ProviderError::Failed("rejected".into()));
    let mut seen = Vec::new();
    for i in 1..=ce_events::MAX_ATTEMPTS {
        r.clock
            .store(1000 + u64::from(i) * 10_000, Ordering::SeqCst);
        seen.push(r.run().await);
    }
    assert_eq!(
        seen.last().unwrap(),
        &json!({"delivered": 0, "retrying": 0, "dead": 1})
    );
    let j = r.job().await;
    assert_eq!(j["status"], "dead");
    assert_eq!(j["attempts"], ce_events::MAX_ATTEMPTS);
    assert_eq!(j["last_error"], "rejected");

    // Dead stays dead: nothing is sent until a person queues it again.
    r.clock.store(900_000, Ordering::SeqCst);
    assert_eq!(
        r.run().await,
        json!({"delivered": 0, "retrying": 0, "dead": 0})
    );
    assert_eq!(
        r.fake.calls.lock().unwrap().len(),
        ce_events::MAX_ATTEMPTS as usize
    );
    r.finish().await;
}

#[tokio::test]
async fn jobs_waiting_for_a_retry_never_hide_a_due_one() {
    let Some(mut r) = rig().await else { return };
    // More waiting jobs than one page holds, then the merge's job is the only due one.
    let mut owner = Api {
        app: r.reviewer.app.clone(),
        n: 0,
        role: "owner",
        changeset: None,
    };
    for i in 0..120 {
        let (s, v) = owner
            .call(
                "POST",
                &format!("/v1/{INST}/archiver__job"),
                Some(json!({"changeset": format!("w{i}"), "status": "queued", "attempts": 1, "not_before": 900_000})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    assert_eq!(
        r.run().await,
        json!({"delivered": 1, "retrying": 0, "dead": 0})
    );
    let done = r
        .reviewer
        .list("archiver__job")
        .await
        .into_iter()
        .filter(|j| j["status"] == "done")
        .count();
    assert_eq!(done, 1);
    tokio::task::spawn_blocking(move || drop(owner))
        .await
        .unwrap();
    r.finish().await;
}
