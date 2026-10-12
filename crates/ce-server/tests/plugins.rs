//! One module, two tenant-private plugins, no engine change: the same inventory module gives
//! different results under different policy packs, end to end through the HTTP API.
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
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
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

fn manifest(p: Option<&ce_plugin::Plugin>) -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let shell = ce_tools::compose(&shell, &["period"]).unwrap();
    ce_plugin::compose(&shell, "business.inventory", p)
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
            json!({"item": id(&item), "period": id(&period), "opening_oh": "10"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{line}");
    let lid = id(&line);
    let pull = api
        .post("stock_movement", json!({"line": lid, "pulled": "12"}))
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
rule = "round(max(0, when(unit_price > 0, unit_price, coalesce(ref(item).unit_price, 0))), 4)"
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
