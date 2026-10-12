//! One module, two tenant-private plugins, no engine change: the same inventory module gives
//! different results under different policy packs, end to end through the HTTP API.
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
const INST: &str = "plug-1";

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
    let m = validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_plug_{}_{}",
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
}

impl Api {
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

fn plugin(id: &str, slots: &str) -> ce_plugin::Plugin {
    let text = format!(
        r#"schema_version = 1
[plugin]
id = "{id}"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
{slots}"#
    );
    ce_plugin::parse(
        &text,
        &ce_plugin::builtin_registry(),
        &semver::Version::new(0, 1, 0),
    )
    .unwrap_or_else(|e| panic!("{e:?}"))
}

const DIGEST: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn manifest(p: Option<&ce_plugin::Plugin>) -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let shell = ce_tools::compose(&shell, &["period", "audit"]).unwrap();
    let pinned = p.map(|p| (p, DIGEST));
    ce_plugin::compose(&shell, "business.inventory", pinned)
        .unwrap()
        .manifest
}

fn id(v: &Value) -> String {
    v["data"]["id"].as_str().unwrap().to_string()
}

/// Same requests, whatever the policy: a priced item, an open period, a count of 10, and a
/// pull of 12 that overdraws it.
async fn run(url: &str, m: Value) -> (Value, (StatusCode, Value)) {
    let mut api = Api {
        app: app(url, m).await,
        n: 0,
    };
    let (s, period) = api
        .post(
            "period",
            json!({"label": "2026-W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{period}");
    let (s, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "Rice 25lb", "unit_price": "1.234568"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{item}");
    let (s, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&period), "opening_oh": "10", "unit_price": "1.234568"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{line}");
    let lid = id(&line);
    let pid = id(&period);
    let pull = api
        .post(
            "stock_movement",
            json!({"line": lid, "period": pid, "pulled": "12"}),
        )
        .await;
    let cur = api.get("stock_line", &lid).await;
    close(api.app).await;
    (cur, pull)
}

#[tokio::test]
async fn one_module_two_plugins_two_outcomes() {
    let Some(url) = url() else { return };

    // Policy A is the module's defaults: price kept to six places, stock never below zero,
    // and a pull that overdraws a line is refused.
    let (line, pull) = run(&url, manifest(None)).await;
    assert_eq!(pull.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", pull.1);
    assert_eq!(line["data"]["price"], "1.234568");
    assert_eq!(line["data"]["ending_qty"], "10.000");
    assert_eq!(line["data"]["ending_value"], "12.35");

    // Policy B is a different plugin on the same module: price to four places, negative stock
    // allowed and shown as it is. No Rust changed between the two runs.
    let b = plugin(
        "lenient-rules",
        r#"[[policy]]
slot = "price"
rule = "round(max(0, unit_price), 4)"
[[policy]]
slot = "ending_qty"
rule = "opening_oh + received - pulled"
[[policy]]
slot = "over_pull"
rule = "true"
"#,
    );
    let (line, pull) = run(&url, manifest(Some(&b))).await;
    assert_eq!(pull.0, StatusCode::CREATED, "{}", pull.1);
    assert_eq!(line["data"]["price"], "1.234600");
    assert_eq!(line["data"]["ending_qty"], "-2.000");
    assert_eq!(line["data"]["ending_value"], "-2.47");
}

#[tokio::test]
async fn a_closed_period_freezes_its_lines_and_movements() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest(None)).await,
        n: 0,
    };
    let (_, p1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, p2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    let (_, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "Rice", "unit_price": "2"}),
        )
        .await;
    let mk_line = |p: &Value| json!({"item": id(&item), "period": id(p), "opening_oh": "10", "unit_price": "2"});
    let (_, l1) = api.post("stock_line", mk_line(&p1)).await;
    let (_, l2) = api.post("stock_line", mk_line(&p2)).await;
    let (s, m1) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "3"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{m1}");
    assert_eq!(
        api.get("stock_line", &id(&l1)).await["data"]["ending_qty"],
        "7.000"
    );
    assert_eq!(
        api.get("stock_line", &id(&l1)).await["data"]["ending_value"],
        "14.00"
    );

    // The item's catalog price changes; the line keeps the price stamped on it.
    let cur = api.get("stock_item", &id(&item)).await;
    let (s, r) = api
        .patch(
            "stock_item",
            &id(&item),
            cur["data"]["version"].as_u64().unwrap(),
            json!({"unit_price": "5"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, m3) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{m3}");
    let line = api.get("stock_line", &id(&l1)).await;
    assert_eq!(
        (&line["data"]["ending_qty"], &line["data"]["ending_value"]),
        (&json!("6.000"), &json!("12.00"))
    );

    // A movement must belong to its line's period.
    let (s, bad) = api
        .post(
            "stock_movement",
            json!({"line": id(&l2), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");

    // Close period 1.
    let (s, closed) = api
        .call(
            "POST",
            &format!("/v1/{INST}/period/{}/close", id(&p1)),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{closed}");

    // Nothing in it can change: not the line, and not a movement added, edited or removed.
    let (s, r) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_ne!(s, StatusCode::CREATED, "{r}");
    let (s, r) = api
        .patch("stock_movement", &id(&m1), 1, json!({"pulled": "9"}))
        .await;
    assert_ne!(s, StatusCode::OK, "{r}");
    assert_ne!(
        api.delete("stock_movement", &id(&m1), 1).await,
        StatusCode::NO_CONTENT
    );
    // A movement of the open period cannot be pointed at the closed one either.
    let (_, m2) = api
        .post(
            "stock_movement",
            json!({"line": id(&l2), "period": id(&p2), "pulled": "1"}),
        )
        .await;
    let (s, r) = api
        .patch(
            "stock_movement",
            &id(&m2),
            1,
            json!({"line": id(&l1), "period": id(&p1)}),
        )
        .await;
    assert_ne!(s, StatusCode::OK, "{r}");
    let l1 = api.get("stock_line", &id(&l1)).await;
    assert_eq!(
        (&l1["data"]["pulled"], &l1["data"]["ending_qty"]),
        (&json!("4.000"), &json!("6.000"))
    );
    close(api.app).await;
}

#[tokio::test]
async fn every_audit_event_names_the_plugin_that_ruled() {
    let Some(url) = url() else { return };
    let b = plugin(
        "lenient-rules",
        "[[policy]]\nslot = \"over_pull\"\nrule = \"true\"\n",
    );
    let mut api = Api {
        app: app(&url, manifest(Some(&b))).await,
        n: 0,
    };
    let (s, _) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, v) = api
        .call("GET", &format!("/v1/{INST}/audit_event"), None, None)
        .await;
    let events = v["data"].as_array().unwrap();
    assert!(!events.is_empty(), "{v}");
    for e in events {
        let d = e;
        assert_eq!(d["provenance"]["plugin"]["id"], "lenient-rules", "{e}");
        assert_eq!(d["provenance"]["plugin"]["digest"], DIGEST, "{e}");
        assert_eq!(d["provenance"]["module"]["name"], "business.inventory");
        assert_eq!(d["manifest_hash"].as_str().unwrap().len(), 64);
    }
    close(api.app).await;
}
