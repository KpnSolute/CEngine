//! `GET /v1/{inst}/_barcode`: symbols as SVG, for any signed-in caller.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, AppState};
use ce_store::MemoryStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::Value;
use sha2::Sha256;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "bc-1";

fn token() -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{T1}","roles":["manager"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn app() -> Router {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let m = validate(v.to_string().as_bytes()).unwrap();
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

async fn get(app: &Router, uri: &str, tok: Option<&str>) -> (StatusCode, String, Option<String>) {
    let mut req = Request::builder().uri(uri);
    if let Some(t) = tok {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let ctype = res
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_string());
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string(), ctype)
}

#[tokio::test]
async fn every_symbology_renders_as_svg() {
    let app = app();
    let t = token();
    for (ty, data) in [
        ("code128", "SKU-0042"),
        ("ean13", "590123412345"),
        ("upca", "03600029145"),
        ("qr", "https://example.com/item/42"),
    ] {
        let (s, body, ctype) = get(
            &app,
            &format!(
                "/v1/{INST}/_barcode?type={ty}&data={}",
                data.replace('/', "%2F")
            ),
            Some(&t),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{ty}: {body}");
        assert_eq!(ctype.as_deref(), Some("image/svg+xml"));
        assert!(body.starts_with("<svg"), "{ty}: {body}");
    }
    let (_, big, _) = get(
        &app,
        &format!("/v1/{INST}/_barcode?type=code128&data=A&module_size=8"),
        Some(&t),
    )
    .await;
    let (_, small, _) = get(
        &app,
        &format!("/v1/{INST}/_barcode?type=code128&data=A&module_size=1"),
        Some(&t),
    )
    .await;
    assert_ne!(big, small, "options change the output");
}

#[tokio::test]
async fn bad_requests_are_refused_with_the_reason() {
    let app = app();
    let t = token();
    for (q, needle) in [
        ("type=nope&data=A", "type"),
        ("type=code128", "data"),
        ("type=ean13&data=5901234123458", "bad_check_digit"),
        ("type=ean13&data=12ab", "bad_"),
        ("type=code128&data=A&module_size=99", "/module_size"),
        ("type=code128&data=A&bar_height=0", "/bar_height"),
        ("type=code128&data=A&quiet_zone=21", "/quiet_zone"),
        ("type=code128&data=A&bar_height=x", "bar_height"),
    ] {
        let (s, body, _) = get(&app, &format!("/v1/{INST}/_barcode?{q}"), Some(&t)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {body}");
        assert!(body.contains(needle), "{q}: {body}");
    }
    // The wrong check digit is not silently fixed.
    let (_, body, _) = get(
        &app,
        &format!("/v1/{INST}/_barcode?type=ean13&data=5901234123458"),
        Some(&t),
    )
    .await;
    assert!(body.contains("should be 7"), "{body}");
}

#[tokio::test]
async fn a_token_for_this_installation_is_required() {
    let app = app();
    let (s, _, _) = get(&app, &format!("/v1/{INST}/_barcode?type=qr&data=x"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = get(&app, "/v1/other/_barcode?type=qr&data=x", Some(&token())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
