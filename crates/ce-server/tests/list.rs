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
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
const INST: &str = "inv-1";

fn token(tenant: &str, roles: &[&str], exp: u64) -> String {
    let list = roles
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(",");
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{tenant}","roles":[{list}],"exp":{exp}}}"#
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

static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Installing runs blocking Postgres calls, so it happens off the async runtime.
async fn app(url: &str) -> Router {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || build(&url))
        .await
        .unwrap()
}

/// Dropping the store closes a blocking connection, so that also happens off the runtime.
async fn close(a: Router) {
    tokio::task::spawn_blocking(move || drop(a)).await.unwrap();
}

fn build(url: &str) -> Router {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let m = validate(std::fs::read(path).unwrap().as_slice()).unwrap();
    let schema = format!(
        "ce_list_{}_{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    tok: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {tok}"))
        .header("idempotency-key", uri);
    if let Some(b) = &body {
        req = req.header(header::CONTENT_TYPE, "application/json");
        let _ = b;
    }
    let res = app
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

#[tokio::test]
async fn list_pages_in_order_and_is_tenant_scoped() {
    let Some(url) = url() else { return };
    let a = app(&url).await;
    let (t1, t2) = (token(T1, &["manager"], 2000), token(T2, &["manager"], 2000));
    for (i, n) in ["bread", "apple", "dates", "cherry", "eggs"]
        .iter()
        .enumerate()
    {
        let uri = format!("/v1/{INST}/item");
        let key = format!("{uri}?{i}");
        let req = Request::builder()
            .method("POST")
            .uri(&uri)
            .header(header::AUTHORIZATION, format!("Bearer {t1}"))
            .header("idempotency-key", key)
            .body(Body::from(
                json!({"name": n, "unit_cost": "1.50"}).to_string(),
            ))
            .unwrap();
        let res = a.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
    // Another tenant's data never shows up.
    let (st, other) = call(&a, "GET", &format!("/v1/{INST}/item"), &t2, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(other["data"].as_array().unwrap().len(), 0);

    let mut names = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..5 {
        let mut uri = format!("/v1/{INST}/item?sort=name&limit=2&select=name,unit_cost");
        if let Some(c) = &after {
            uri.push_str(&format!("&after={c}"));
        }
        let (st, page) = call(&a, "GET", &uri, &t1, None).await;
        assert_eq!(st, StatusCode::OK, "{page}");
        for r in page["data"].as_array().unwrap() {
            assert_eq!(r["unit_cost"], "1.5000");
            names.push(r["name"].as_str().unwrap().to_string());
        }
        match page["meta"]["next"].as_str() {
            Some(c) => after = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(names, ["apple", "bread", "cherry", "dates", "eggs"]);
    close(a).await;
}

#[tokio::test]
async fn bad_list_requests_are_422_problems_naming_the_parameter() {
    let Some(url) = url() else { return };
    let a = app(&url).await;
    let t1 = token(T1, &["manager"], 2000);
    for (q, path, code) in [
        ("sort=unit_cost", "sort", "unindexed_sort"),
        ("select=nope", "select", "unknown_field"),
        ("limit=abc", "limit", "invalid_value"),
        ("filter=%7Bnot", "filter", "invalid_value"),
        ("after=%21%21", "after", "invalid_value"),
    ] {
        let (st, p) = call(&a, "GET", &format!("/v1/{INST}/item?{q}"), &t1, None).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {p}");
        assert_eq!(p["errors"][0]["path"], path, "{q}");
        assert_eq!(p["errors"][0]["code"], code, "{q}");
    }
    // No token, no list.
    let req = Request::builder()
        .uri(format!("/v1/{INST}/item"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        a.clone().oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    close(a).await;
}
