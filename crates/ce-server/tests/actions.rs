//! Named actions: set, transition, create and emit in one transaction, with state and role checks.
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
const INST: &str = "ac-1";

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
        "ce_ac_{}_{}",
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
                "order": {
                    "fields": {
                        "name": {"type": "string", "required": true},
                        "note": {"type": "string"},
                        "sent_on": {"type": "date"},
                        "amount": {"type": "decimal", "precision": 12, "scale": 2, "default": 0},
                        "account": {"type": "ref", "ref": "account"}
                    },
                    "state": {"field": "status", "initial": "draft", "states": {
                        "draft": {"code": 100, "to": ["sent"]},
                        "sent": {"code": 200, "to": ["paid"]},
                        "paid": {"code": 600}
                    }}
                },
                "task": {"fields": {"title": {"type": "string", "required": true}}},
                "account": {
                    "fields": {
                        "name": {"type": "string", "required": true},
                        "balance": {"type": "decimal", "precision": 12, "scale": 2, "default": 0}
                    },
                    "invariants": [{"name": "not_overdrawn", "expr": "balance >= 0", "message_code": "account.overdrawn"}]
                }
            },
            "actions": {
                "send": {
                    "entity": "order",
                    "input": {"memo": {"type": "string", "required": true, "max_length": 20}},
                    "requires_state": ["draft"],
                    "effect": [
                        {"set": {"note": "input.memo", "sent_on": "today"}},
                        {"transition": "sent"},
                        {"create": {"entity": "task", "values": {"title": "name"}}},
                        {"emit": "order.sent"}
                    ]
                },
                "pay": {"entity": "order", "effect": [{"transition": "paid"}]},
                "credit": {
                    "entity": "order",
                    "effect": [
                        {"update": {"entity": "account", "id": "account", "set": {"balance": "ref(account).balance + amount"}}}
                    ]
                },
                "bill": {
                    "entity": "order",
                    "effect": [
                        {"set": {"note": "'billed'"}},
                        {"when": "amount > 100", "set": {"note": "'large'"}},
                        {"when": "amount > 100", "update": {"entity": "account", "id": "account", "set": {"balance": "ref(account).balance - amount"}}},
                        {"when": "amount <= 100", "set": {"sent_on": "today"}}
                    ]
                }
            },
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "clerk": {"read": ["*"], "write": [], "actions": ["send"]}
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

fn as_role(a: &Api, role: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        n: 1000,
        role,
        extra: None,
    }
}

impl Api {
    async fn act(
        &mut self,
        name: &str,
        id: &str,
        version: u64,
        input: Value,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/actions/{name}"),
            Some(version),
            Some(json!({"id": id, "input": input})),
        )
        .await
    }
}

#[tokio::test]
async fn an_action_sets_moves_creates_and_audits_in_one_transaction() {
    let Some(mut owner) = setup().await else {
        return;
    };
    let mut clerk = as_role(&owner, "clerk");
    let (s, o) = owner.post("order", json!({"name": "rent"})).await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    let id = o["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(o["data"]["status"], "draft");

    // The clerk has no write on orders but may run `send`.
    let (s, v) = clerk.act("send", &id, 1, json!({"memo": "thanks"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "sent");
    assert_eq!(v["data"]["note"], "thanks");
    assert_eq!(v["data"]["sent_on"], "1970-01-01");
    assert_eq!(v["meta"]["emitted"][0], "order.sent");
    assert_eq!(v["meta"]["created"][0]["entity"], "task");
    let tasks = owner.list("task").await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["title"], "rent");
    // create + (set, transition, task create) = 4 audit rows.
    assert_eq!(owner.list("audit_event").await.len(), 4);

    // Wrong state, wrong version, missing role and bad input leave everything as it was.
    let version = v["data"]["version"].as_u64().unwrap();
    let (s, v) = clerk
        .act("send", &id, version, json!({"memo": "again"}))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "invalid_transition");
    let (s, _) = owner.act("pay", &id, 1, json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = clerk.act("pay", &id, version, json!({})).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = owner.act("pay", &id, version, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "paid");
    let (s, v) = owner.act("pay", &id, version + 1, json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, _) = owner.act("nothing", &id, 1, json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    close(vec![owner.app, clerk.app]).await;
}

#[tokio::test]
async fn a_failing_action_changes_nothing() {
    let Some(mut owner) = setup().await else {
        return;
    };
    let (_, o) = owner.post("order", json!({"name": "lease"})).await;
    let id = o["data"]["id"].as_str().unwrap().to_string();
    // Input too long: refused before anything is written.
    let (s, v) = owner
        .act("send", &id, 1, json!({"memo": "x".repeat(40)}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, v) = owner.act("send", &id, 1, json!({})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, v) = owner
        .act("send", &id, 1, json!({"memo": "a", "extra": 1}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (_, got) = owner
        .call("GET", &format!("/v1/{INST}/order/{id}"), None, None)
        .await;
    assert_eq!(got["data"]["status"], "draft");
    assert_eq!(got["data"]["version"], 1);
    assert!(owner.list("task").await.is_empty());
    close(vec![owner.app]).await;
}

#[tokio::test]
async fn effects_run_only_when_their_condition_holds_and_can_update_another_record() {
    let Some(mut owner) = setup().await else {
        return;
    };
    let (_, acc) = owner
        .post("account", json!({"name": "petty", "balance": "500.00"}))
        .await;
    let acc_id = acc["data"]["id"].as_str().unwrap().to_string();
    let (s, big) = owner
        .post(
            "order",
            json!({"name": "oven", "amount": "120.00", "account": acc_id}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{big}");
    let (s, small) = owner
        .post(
            "order",
            json!({"name": "mop", "amount": "20.00", "account": acc_id}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{small}");

    let big_id = big["data"]["id"].as_str().unwrap();
    let (s, v) = owner.act("bill", big_id, 1, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["note"], "large");
    assert!(
        v["data"]["sent_on"].is_null(),
        "the small-order effect must not run"
    );
    let (_, a) = owner
        .call("GET", &format!("/v1/{INST}/account/{acc_id}"), None, None)
        .await;
    assert_eq!(a["data"]["balance"], "380.00");
    assert_eq!(a["data"]["version"], 2);

    let small_id = small["data"]["id"].as_str().unwrap();
    let (s, v) = owner.act("bill", small_id, 1, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["note"], "billed");
    assert_eq!(v["data"]["sent_on"], "1970-01-01");
    let (_, a) = owner
        .call("GET", &format!("/v1/{INST}/account/{acc_id}"), None, None)
        .await;
    assert_eq!(
        a["data"]["balance"], "380.00",
        "a skipped update changes nothing"
    );

    // The account's own rule applies to the update, and the whole action is undone.
    let (_, poor) = owner
        .post("account", json!({"name": "tiny", "balance": "10.00"}))
        .await;
    let poor_id = poor["data"]["id"].as_str().unwrap().to_string();
    let (_, o) = owner
        .post(
            "order",
            json!({"name": "van", "amount": "900.00", "account": poor_id}),
        )
        .await;
    let oid = o["data"]["id"].as_str().unwrap();
    let (s, v) = owner.act("bill", oid, 1, json!({})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (_, o2) = owner
        .call("GET", &format!("/v1/{INST}/order/{oid}"), None, None)
        .await;
    assert!(
        o2["data"]["note"].is_null(),
        "the first set was undone: {o2}"
    );
    let (_, p) = owner
        .call("GET", &format!("/v1/{INST}/account/{poor_id}"), None, None)
        .await;
    assert_eq!(p["data"]["balance"], "10.00");

    // No account on the order: the update has nothing to change and the action fails whole.
    let (_, lone) = owner
        .post("order", json!({"name": "box", "amount": "500.00"}))
        .await;
    let lid = lone["data"]["id"].as_str().unwrap();
    let (s, v) = owner.act("bill", lid, 1, json!({})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    // An action that only updates another record still advances its own, so a retry with the same
    // version cannot apply the update twice.
    let (_, c) = owner
        .post(
            "order",
            json!({"name": "refund", "amount": "50.00", "account": acc_id}),
        )
        .await;
    let cid = c["data"]["id"].as_str().unwrap();
    let (s, v) = owner.act("credit", cid, 1, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["version"], 2);
    let (s, v) = owner.act("credit", cid, 1, json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (_, a) = owner
        .call("GET", &format!("/v1/{INST}/account/{acc_id}"), None, None)
        .await;
    assert_eq!(a["data"]["balance"], "430.00", "credited once");
    close(vec![owner.app]).await;
}
