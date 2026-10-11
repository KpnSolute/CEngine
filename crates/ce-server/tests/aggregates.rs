//! Totals over child records stay consistent: a write to a child updates its parents in the
//! same transaction, and a rule broken anywhere keeps the whole write out.
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
const INST: &str = "agg-1";

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
        "ce_agg_{}_{}",
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
    async fn get(&mut self, entity: &str, id: &str) -> Value {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/{entity}/{id}"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }
    async fn patch(
        &mut self,
        entity: &str,
        id: &str,
        version: u64,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(
            "PATCH",
            &format!("/v1/{INST}/{entity}/{id}"),
            Some(version),
            Some(body),
        )
        .await
    }
    async fn delete(&mut self, entity: &str, id: &str, version: u64) -> StatusCode {
        self.call(
            "DELETE",
            &format!("/v1/{INST}/{entity}/{id}"),
            Some(version),
            None,
        )
        .await
        .0
    }
}

fn invoicing() -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "shop", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    ce_tools::compose(&shell, &["invoicing"]).unwrap()
}

fn id(v: &Value) -> String {
    v["data"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn invoice_totals_follow_their_lines() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, invoicing()).await,
        n: 0,
    };
    let (s, inv) = api
        .post("invoice", json!({"number": "INV-1", "bill_to_name": "Ada", "currency": "USD", "issue_date": "2026-11-01"}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    assert_eq!(
        (&inv["data"]["subtotal"], &inv["data"]["total"]),
        (&json!("0.00"), &json!("0.00"))
    );
    let iid = id(&inv);

    // A line computes its own total, and the invoice follows.
    let (s, l1) = api
        .post("invoice_line", json!({"invoice": iid, "description": "work", "quantity": "2.5", "unit_price": "10.00"}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{l1}");
    assert_eq!(l1["data"]["line_total"], "25.00");
    let cur = api.get("invoice", &iid).await;
    assert_eq!(
        (&cur["data"]["subtotal"], &cur["data"]["total"]),
        (&json!("25.00"), &json!("25.00"))
    );
    assert_eq!(cur["data"]["version"], 2);

    // Rounds half up per line.
    let (_, l2) = api
        .post("invoice_line", json!({"invoice": iid, "description": "bolt", "quantity": "0.125", "unit_price": "0.20"}))
        .await;
    assert_eq!(l2["data"]["line_total"], "0.03");
    assert_eq!(api.get("invoice", &iid).await["data"]["subtotal"], "25.03");

    // Tax is the caller's; the total adds it.
    let cur = api.get("invoice", &iid).await;
    let (s, p) = api
        .patch(
            "invoice",
            &iid,
            cur["data"]["version"].as_u64().unwrap(),
            json!({"tax": "2.00"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(
        (&p["data"]["subtotal"], &p["data"]["total"]),
        (&json!("25.03"), &json!("27.03"))
    );

    // Changing a line changes the invoice, tax included.
    let (s, p) = api
        .patch("invoice_line", &id(&l1), 1, json!({"quantity": "4"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["data"]["line_total"], "40.00");
    let cur = api.get("invoice", &iid).await;
    assert_eq!(
        (&cur["data"]["subtotal"], &cur["data"]["total"]),
        (&json!("40.03"), &json!("42.03"))
    );

    // Deleting a line removes it from the total.
    assert_eq!(
        api.delete("invoice_line", &id(&l2), 1).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.get("invoice", &iid).await["data"]["subtotal"], "40.00");

    // Moving a line to another invoice updates both.
    let (_, other) = api
        .post("invoice", json!({"number": "INV-2", "bill_to_name": "Bo", "currency": "USD", "issue_date": "2026-11-01"}))
        .await;
    let oid = id(&other);
    let l1v = api.get("invoice_line", &id(&l1)).await["data"]["version"]
        .as_u64()
        .unwrap();
    let (s, p) = api
        .patch("invoice_line", &id(&l1), l1v, json!({"invoice": oid}))
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(api.get("invoice", &iid).await["data"]["subtotal"], "0.00");
    assert_eq!(api.get("invoice", &oid).await["data"]["subtotal"], "40.00");

    // A caller cannot write a derived total.
    let (s, _) = api
        .patch("invoice", &oid, 2, json!({"subtotal": "1.00"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    close(api.app).await;
}

fn capped() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "capped", "version": "0.1.0"},
        "entities": {
            "customer": {"fields": {
                "name": {"type": "string"},
                "owed": {"type": "decimal", "precision": 12, "scale": 2, "computed": "sum(children(order, customer).total)"},
                "orders": {"type": "integer", "computed": "count(children(order, customer))"}
            }, "invariants": [{"name": "credit", "expr": "owed <= 100", "message_code": "customer.over_credit"}]},
            "order": {"fields": {
                "customer": {"type": "ref", "ref": "customer", "required": true},
                "total": {"type": "decimal", "precision": 12, "scale": 2, "computed": "sum(children(item, order).price)"}
            }},
            "item": {"fields": {
                "order": {"type": "ref", "ref": "order", "required": true},
                "price": {"type": "decimal", "precision": 12, "scale": 2, "required": true}
            }}
        },
        "roles": {"owner": {"read": ["*"], "write": ["*"]}}
    })
}

#[tokio::test]
async fn totals_cascade_and_a_broken_rule_keeps_the_whole_write_out() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, capped()).await,
        n: 0,
    };
    let (s, c) = api.post("customer", json!({"name": "Ada"})).await;
    assert_eq!(s, StatusCode::CREATED, "{c}");
    let cid = id(&c);
    assert_eq!(
        (&c["data"]["owed"], &c["data"]["orders"]),
        (&json!("0.00"), &json!(0))
    );
    let (_, o1) = api.post("order", json!({"customer": cid})).await;
    let (_, o2) = api.post("order", json!({"customer": cid})).await;
    let (o1, o2) = (id(&o1), id(&o2));
    let cust = api.get("customer", &cid).await;
    assert_eq!(cust["data"]["orders"], 2);

    // item -> order -> customer in one write.
    let (s, i1) = api
        .post("item", json!({"order": o1, "price": "60.00"}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{i1}");
    assert_eq!(api.get("order", &o1).await["data"]["total"], "60.00");
    assert_eq!(api.get("customer", &cid).await["data"]["owed"], "60.00");

    // This item would take the customer over 100: the item, the order and the customer all stay.
    let before = (
        api.get("order", &o2).await["data"].clone(),
        api.get("customer", &cid).await["data"].clone(),
    );
    let (s, e) = api
        .post("item", json!({"order": o2, "price": "50.00"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert!(e.to_string().contains("customer.over_credit"), "{e}");
    assert_eq!(api.get("order", &o2).await["data"], before.0);
    assert_eq!(api.get("customer", &cid).await["data"], before.1);
    let (s, list) = api
        .call("GET", &format!("/v1/{INST}/item"), None, None)
        .await;
    if s == StatusCode::OK {
        assert_eq!(list["data"].as_array().map(Vec::len), Some(1), "{list}");
    }

    // Within the limit it goes through, and deleting an item frees credit again.
    let (s, i2) = api
        .post("item", json!({"order": o2, "price": "40.00"}))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{i2}");
    assert_eq!(api.get("customer", &cid).await["data"]["owed"], "100.00");
    assert_eq!(
        api.delete("item", &id(&i1), 1).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.get("customer", &cid).await["data"]["owed"], "40.00");

    // A patch that breaks the rule is refused and changes nothing.
    let i2v = api.get("item", &id(&i2)).await["data"]["version"]
        .as_u64()
        .unwrap();
    let (s, _) = api
        .patch("item", &id(&i2), i2v, json!({"price": "500.00"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(api.get("item", &id(&i2)).await["data"]["price"], "40.00");
    assert_eq!(api.get("customer", &cid).await["data"]["owed"], "40.00");
    close(api.app).await;
}

fn tiered() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "tiered", "version": "0.1.0"},
        "entities": {
            "customer": {"fields": {"tier": {"type": "string"}}},
            "order": {"fields": {
                "customer": {"type": "ref", "ref": "customer", "required": true},
                "tier": {"type": "string", "computed": "ref(customer).tier"}
            }}
        },
        "roles": {"owner": {"read": ["*"], "write": ["*"]}}
    })
}

#[tokio::test]
async fn a_retry_replays_even_when_a_referenced_record_changed_meanwhile() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, tiered()).await,
        n: 0,
    };
    let (_, c) = api.post("customer", json!({"tier": "gold"})).await;
    let cid = id(&c);
    let uri = format!("/v1/{INST}/order");
    let body = json!({"customer": cid});
    let (s, first) = api
        .call_keyed("POST", &uri, None, Some(body.clone()), "retry-1")
        .await;
    assert_eq!(s, StatusCode::CREATED, "{first}");
    assert_eq!(first["data"]["tier"], "gold");
    // The customer changes, then the client retries the identical request.
    let (s, _) = api
        .patch(
            "customer",
            &cid,
            c["data"]["version"].as_u64().unwrap(),
            json!({"tier": "silver"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, again) = api
        .call_keyed("POST", &uri, None, Some(body), "retry-1")
        .await;
    assert_eq!(s, StatusCode::CREATED, "{again}");
    assert_eq!(again["data"], first["data"]);
    // A different body under the same key is still refused.
    let (s, _) = api
        .call_keyed(
            "POST",
            &uri,
            None,
            Some(json!({"customer": id(&c), "tier": "x"})),
            "retry-1",
        )
        .await;
    assert_ne!(s, StatusCode::CREATED);
    close(api.app).await;
}
