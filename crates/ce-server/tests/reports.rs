//! Declared reports: grouped measures as JSON, CSV and print-ready HTML.
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
const T2: &str = "22222222-2222-2222-2222-222222222222";
const INST: &str = "rp-1";

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

fn build(url: &str, m: Value) -> Router {
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_rp_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

/// Dropping the last handle closes the database client, which blocks: do it off the runtime.
struct Api {
    app: Option<Router>,
    role: &'static str,
    tenant: &'static str,
    n: u32,
}

impl Drop for Api {
    fn drop(&mut self) {
        if let Some(app) = self.app.take() {
            std::thread::spawn(move || drop(app)).join().unwrap();
        }
    }
}

impl Api {
    async fn go(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        self.n += 1;
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(self.tenant, self.role, self.role)),
            )
            .header(
                "idempotency-key",
                format!("k{}-{}-{}", self.tenant, self.role, self.n),
            );
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
                req.body(Body::from(body.unwrap_or("").to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }
    async fn send(
        &mut self,
        method: &str,
        uri: &str,
        body: Option<&str>,
    ) -> (StatusCode, String, Value) {
        let (s, _, t) = self.go(method, uri, None, body).await;
        let j = serde_json::from_str(&t).unwrap_or(Value::Null);
        (s, t, j)
    }
    async fn call(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
    ) -> (StatusCode, String, Value) {
        let (s, _, t) = self.go(method, uri, version, None).await;
        let j = serde_json::from_str(&t).unwrap_or(Value::Null);
        (s, t, j)
    }
    /// (status, content-security-policy header, body)
    async fn raw(&mut self, method: &str, uri: &str) -> (StatusCode, String, String) {
        let (s, h, t) = self.go(method, uri, None, None).await;
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (s, csp, t)
    }
}

fn manifest() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
        "entities": {"sale": {
            "fields": {
                "category": {"type": "string", "indexed": true},
                "region": {"type": "string", "indexed": true},
                "qty": {"type": "integer", "indexed": true},
                "amount": {"type": "decimal", "precision": 14, "scale": 2},
                "day": {"type": "date", "indexed": true}
            },
            "sort_keys": ["category", "qty"]
        }},
        "reports": {"by_category": {
            "entity": "sale",
            "title": "Sales <by> category",
            "group_by": ["category"],
            "measures": [
                {"fn": "count", "as": "n"},
                {"fn": "sum", "field": "qty", "as": "units"},
                {"fn": "sum", "field": "amount", "as": "total"},
                {"fn": "avg", "field": "amount", "as": "mean"},
                {"fn": "min", "field": "day", "as": "first_day"},
                {"fn": "max", "field": "amount", "as": "top"}
            ],
            "filter": "qty > 0",
            "params": ["region"]
        }, "everything": {
            "entity": "sale",
            "measures": [{"fn": "count", "as": "n"}, {"fn": "sum", "field": "amount", "as": "total"}]
        }},
        "roles": {
            "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
            "nobody": {"read": [], "write": [], "actions": []}
        }
    })
}

async fn setup(role: &'static str) -> Option<Api> {
    let url = url()?;
    let app = tokio::task::spawn_blocking(move || build(&url, manifest()))
        .await
        .unwrap();
    Some(Api {
        app: Some(app),
        role,
        tenant: T1,
        n: 0,
    })
}

fn as_role(a: &Api, role: &'static str, tenant: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        role,
        tenant,
        n: 1000,
    }
}

async fn sale(a: &mut Api, body: Value) -> String {
    let (s, _, v) = a
        .send("POST", &format!("/v1/{INST}/sale"), Some(&body.to_string()))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v["data"]["id"].as_str().unwrap().to_string()
}

async fn seed(a: &mut Api) {
    sale(a, json!({"category": "soup", "region": "n", "qty": 2, "amount": "12.50", "day": "2026-09-03"})).await;
    sale(
        a,
        json!({"category": "soup", "region": "s", "qty": 3, "amount": "0.25", "day": "2026-09-01"}),
    )
    .await;
    sale(
        a,
        json!({"category": "stew", "region": "n", "qty": 1, "amount": "7.00", "day": "2026-09-09"}),
    )
    .await;
    // No category: a group of its own, last.
    sale(a, json!({"region": "n", "qty": 5, "amount": "1.00"})).await;
    // Excluded by the report's filter (qty > 0) and by a soft delete.
    sale(
        a,
        json!({"category": "soup", "region": "n", "qty": 0, "amount": "100.00"}),
    )
    .await;
    let gone = sale(
        a,
        json!({"category": "soup", "region": "n", "qty": 9, "amount": "900.00"}),
    )
    .await;
    let (s, _, _) = a
        .call("DELETE", &format!("/v1/{INST}/sale/{gone}"), Some(1))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn groups_are_computed_exactly_and_only_for_this_tenant() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    seed(&mut a).await;
    // Another tenant's rows never count.
    let mut other = as_role(&a, "owner", T2);
    sale(
        &mut other,
        json!({"category": "soup", "qty": 1, "amount": "1000.00"}),
    )
    .await;

    let (s, _, v) = a
        .send("GET", &format!("/v1/{INST}/_report/by_category"), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["data"]["columns"],
        json!([
            "category",
            "n",
            "units",
            "total",
            "mean",
            "first_day",
            "top"
        ])
    );
    let rows = v["data"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "{v}");
    assert_eq!(rows[0]["category"], "soup");
    assert_eq!(rows[0]["n"], 2);
    assert_eq!(rows[0]["units"], "5");
    assert_eq!(rows[0]["total"], "12.75");
    assert_eq!(rows[0]["first_day"], "2026-09-01");
    assert_eq!(rows[0]["top"], "12.50");
    assert_eq!(rows[1]["category"], "stew");
    assert_eq!(rows[2]["category"], Value::Null, "the empty group is last");
    assert_eq!(rows[2]["total"], "1.00");

    // A parameter narrows the report.
    let (_, _, v) = a
        .send(
            "GET",
            &format!("/v1/{INST}/_report/by_category?region=n"),
            None,
        )
        .await;
    let rows = v["data"]["rows"].as_array().unwrap();
    assert_eq!(rows[0]["n"], 1);
    assert_eq!(rows[0]["total"], "12.50");

    // No groups: one row over everything live (the deleted one is gone).
    let (_, _, v) = a
        .send("GET", &format!("/v1/{INST}/_report/everything"), None)
        .await;
    assert_eq!(v["data"]["rows"][0]["n"], 5);
    assert_eq!(v["data"]["rows"][0]["total"], "120.75");
}

#[tokio::test]
async fn csv_and_html_are_safe_to_open() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    sale(
        &mut a,
        json!({"category": "=cmd|' /c calc'!A1", "qty": 1, "amount": "-2.50"}),
    )
    .await;
    sale(
        &mut a,
        json!({"category": "<script>alert(1)</script>", "qty": 1, "amount": "1.00"}),
    )
    .await;
    let (s, csv, _) = a
        .send(
            "GET",
            &format!("/v1/{INST}/_report/by_category?format=csv"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        csv.starts_with("category,n,units,total,mean,first_day,top\r\n"),
        "{csv}"
    );
    assert!(csv.contains("'=cmd"), "the formula is guarded: {csv}");
    assert!(
        csv.contains(",-2.50,"),
        "a negative decimal is not mangled: {csv}"
    );

    let res = a
        .raw(
            "GET",
            &format!("/v1/{INST}/_report/by_category?format=html"),
        )
        .await;
    assert_eq!(res.0, StatusCode::OK);
    assert!(res.1.contains("default-src 'none'"), "{:?}", res.1);
    let html = res.2;
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
    assert!(
        html.contains("Sales &lt;by&gt; category"),
        "the title is escaped: {html}"
    );
}

#[tokio::test]
async fn access_and_bad_requests() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    let (s, _, _) = a
        .send("GET", &format!("/v1/{INST}/_report/nope"), None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = a
        .send(
            "GET",
            &format!("/v1/{INST}/_report/by_category?format=pdf"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let mut nobody = as_role(&a, "nobody", T1);
    let (s, _, _) = nobody
        .send("GET", &format!("/v1/{INST}/_report/by_category"), None)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "reading the entity is required");
}
