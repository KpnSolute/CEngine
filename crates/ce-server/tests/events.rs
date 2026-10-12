//! Events out: stored with the change, delivered signed and in order, retried, dead-lettered, replayed.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_events::Signer;
use ce_manifest::validate;
use ce_server::{router, AppState, Deliver, Delivery, Relay};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "ev-1";

fn token(tenant: &str, role: &str, sub: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{sub}","tenant":"{tenant}","roles":["{role}"],"exp":9000000}}"#
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

/// (type, payload, key id, signature) of what was sent.
type Sent = (String, Vec<u8>, String, String);

#[derive(Clone, Default)]
struct Wire {
    sent: Arc<Mutex<Vec<Sent>>>,
    down: Arc<AtomicBool>,
    clock: Arc<AtomicU64>,
}

impl Deliver for Wire {
    fn deliver(&self, d: &Delivery) -> Result<(), String> {
        if self.down.load(Ordering::SeqCst) {
            return Err("receiver down".into());
        }
        self.sent.lock().unwrap().push((
            d.event_type.clone(),
            d.payload.to_vec(),
            d.key_id.to_string(),
            d.signature.to_string(),
        ));
        Ok(())
    }
}

fn build(url: &str, m: Value, wire: Wire) -> Router {
    let clock = wire.clock.clone();
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_ev_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST).with_relay(Relay {
        signer: Signer::new("k1", KEY).unwrap(),
        deliverer: Box::new(wire),
    });
    let clock = clock.clone();
    st.now = std::sync::Arc::new(move || clock.load(Ordering::SeqCst));
    router(st)
}

async fn app(url: &str, m: Value, wire: Wire) -> Router {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || build(&url, m, wire))
        .await
        .unwrap()
}

/// Dropping the last handle closes the database client, which blocks: do it off the runtime.
impl Drop for Api {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            std::thread::spawn(move || drop(app)).join().unwrap();
        }
    }
}

struct Api {
    app: Option<Router>,
    n: u32,
    role: &'static str,
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
        if let Some(v) = version {
            req = req.header("if-match", format!("\"{v}\""));
        }
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let res = self
            .app
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
    async fn relay(&mut self) -> (StatusCode, Value) {
        self.call("POST", &format!("/v1/{INST}/events/relay"), None, None)
            .await
    }
    async fn replay(&mut self, id: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/events/{id}/replay"),
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
            "entities": {},
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "clerk": {"read": ["*"], "write": [], "actions": []},
                "closer": {"read": ["*"], "write": ["period"], "actions": ["close_period"]}
            }
        }),
        &["audit", "period", "events"],
    )
    .unwrap()
}

fn as_role(a: &Api, role: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        n: 1000,
        role,
    }
}

async fn setup() -> Option<(Api, Wire)> {
    let url = url()?;
    let wire = Wire::default();
    wire.clock.store(1000, Ordering::SeqCst);
    let app = app(&url, manifest(), wire.clone()).await;
    Some((
        Api {
            app: Some(app),
            n: 0,
            role: "closer",
        },
        wire,
    ))
}

async fn period(a: &mut Api, label: &str) -> String {
    let (s, p) = a
        .post(
            "period",
            json!({"label": label, "starts_on": "2026-09-01", "ends_on": "2026-09-30"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    p["data"]["id"].as_str().unwrap().to_string()
}

fn statuses(evs: &[Value]) -> Vec<(String, String)> {
    evs.iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                e["status"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_change_stores_its_event_and_the_relay_delivers_it_signed() {
    let Some((mut a, wire)) = setup().await else {
        return;
    };
    let p = period(&mut a, "2026-09").await;
    assert_eq!(a.list("outbox_event").await.len(), 0);
    let (s, v) = a.verb(&p, "close").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let evs = a.list("outbox_event").await;
    assert_eq!(
        statuses(&evs),
        [("period.closed".to_string(), "pending".to_string())]
    );

    let mut owner = as_role(&a, "owner");
    let (s, v) = owner.relay().await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["delivered"], 1);
    assert_eq!(
        statuses(&owner.list("outbox_event").await),
        [("period.closed".to_string(), "delivered".to_string())]
    );
    let sent = wire.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let (ty, payload, key_id, sig) = &sent[0];
    assert_eq!(ty, "com.kpnsolute.compute.period.closed.v1");
    Signer::new("k1", KEY)
        .unwrap()
        .verify(key_id, payload, sig, 1000)
        .unwrap();
    let ce: Value = serde_json::from_slice(payload).unwrap();
    assert_eq!(ce["tenant"], T1);
    assert_eq!(ce["subject"], p);
    assert_eq!(ce["data"]["period"]["status"], "closed");

    // Nothing is left to send.
    let (_, v) = owner.relay().await;
    assert_eq!(v["data"]["delivered"], 0);
}

#[tokio::test]
async fn a_failed_delivery_backs_off_and_keeps_order() {
    let Some((mut a, wire)) = setup().await else {
        return;
    };
    let p1 = period(&mut a, "2026-08").await;
    let p2 = period(&mut a, "2026-09").await;
    a.verb(&p1, "close").await;
    a.verb(&p2, "close").await;
    let mut owner = as_role(&a, "owner");

    wire.down.store(true, Ordering::SeqCst);
    let (_, v) = owner.relay().await;
    assert_eq!(v["data"]["retrying"], 1, "{v}");
    assert_eq!(v["data"]["delivered"], 0);
    let evs = owner.list("outbox_event").await;
    let failed: Vec<&Value> = evs.iter().filter(|e| e["attempts"] == 1).collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["last_error"], "receiver down");

    // Receiver is back, but the first event is still backing off, so the second waits too.
    wire.down.store(false, Ordering::SeqCst);
    let (_, v) = owner.relay().await;
    assert_eq!(v["data"]["delivered"], 0, "{v}");
    assert!(wire.sent.lock().unwrap().is_empty());

    wire.clock.store(1100, Ordering::SeqCst);
    let (_, v) = owner.relay().await;
    assert_eq!(v["data"]["delivered"], 2, "{v}");
    let sent = wire.sent.lock().unwrap().clone();
    let subjects: Vec<Value> = sent
        .iter()
        .map(|s| serde_json::from_slice::<Value>(&s.1).unwrap()["subject"].clone())
        .collect();
    assert_eq!(subjects, [json!(p1), json!(p2)]);
}

#[tokio::test]
async fn an_event_that_keeps_failing_is_dead_lettered_and_can_be_replayed() {
    let Some((mut a, wire)) = setup().await else {
        return;
    };
    let p = period(&mut a, "2026-09").await;
    a.verb(&p, "close").await;
    let mut owner = as_role(&a, "owner");
    wire.down.store(true, Ordering::SeqCst);
    for round in 1..=5u64 {
        wire.clock.store(1000 + round * 10_000, Ordering::SeqCst);
        owner.relay().await;
    }
    let evs = owner.list("outbox_event").await;
    assert_eq!(evs[0]["status"], "dead_lettered");
    assert_eq!(evs[0]["attempts"], 5);
    let eid = evs[0]["id"].as_str().unwrap().to_string();

    // A pending event cannot be replayed; a dead-lettered one can.
    wire.down.store(false, Ordering::SeqCst);
    let (s, v) = owner.replay(&eid).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "pending");
    assert_eq!(v["data"]["attempts"], 0);
    let (s, _) = owner.replay(&eid).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (_, v) = owner.relay().await;
    assert_eq!(v["data"]["delivered"], 1, "{v}");
    let (s, _) = owner.replay(&eid).await;
    assert_eq!(s, StatusCode::OK, "a delivered event may be sent again");
}

#[tokio::test]
async fn events_are_written_by_the_server_and_managed_by_those_allowed() {
    let Some((mut a, _wire)) = setup().await else {
        return;
    };
    let mut owner = as_role(&a, "owner");
    let (s, _) = owner
        .post("outbox_event", json!({"name": "x.y", "at": 1, "seq": 1}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = a.relay().await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "the closer role has no manage_events"
    );
    let mut clerk = as_role(&a, "clerk");
    let (s, _) = clerk.replay("00000000-0000-0000-0000-000000000000").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = owner.replay("00000000-0000-0000-0000-000000000000").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
