//! `POST /v1/{inst}/_auth/token`: a host's login becomes a CEngine token for one fixed tenant.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, AppState, Identity, Login, LoginError};
use ce_store::MemoryStore;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "lg-1";

/// Staff by PIN; three wrong guesses lock everyone out (a stand-in for the host's rate limit).
struct Pins {
    wrong: AtomicU32,
}

impl Login for Pins {
    fn login(&self, c: &Map<String, Value>) -> Result<Identity, LoginError> {
        if self.wrong.load(Ordering::SeqCst) >= 3 {
            return Err(LoginError::Throttled);
        }
        let ok = |sub: &str, roles: &[&str]| Identity {
            sub: sub.into(),
            roles: roles.iter().map(|r| r.to_string()).collect(),
        };
        match (
            c.get("user").and_then(Value::as_str),
            c.get("pin").and_then(Value::as_str),
        ) {
            (Some("ann"), Some("1234")) => Ok(ok("ann", &["staff", "staff", "ghost"])),
            (Some("bo"), Some("4321")) => Ok(ok("bo", &["ghost"])),
            (Some("down"), _) => Err(LoginError::Unavailable),
            _ => {
                self.wrong.fetch_add(1, Ordering::SeqCst);
                Err(LoginError::Denied)
            }
        }
    }
}

fn build(with_login: bool) -> Router {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let m = validate(v.to_string().as_bytes()).unwrap();
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    if with_login {
        st = st
            .with_login(
                Box::new(Pins {
                    wrong: AtomicU32::new(0),
                }),
                T1,
                600,
            )
            .unwrap();
    }
    router(st)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn login(app: &Router, body: &str) -> impl std::future::Future<Output = (StatusCode, Value)> {
    let (app, body) = (app.clone(), body.to_string());
    async move {
        call(
            &app,
            "POST",
            &format!("/v1/{INST}/_auth/token"),
            None,
            &body,
        )
        .await
    }
}

#[tokio::test]
async fn a_good_login_gets_a_working_token_for_the_fixed_tenant() {
    let app = build(true);
    // A tenant sent by the caller is ignored: the host's configuration decides.
    let (s, v) = login(
        &app,
        r#"{"user":"ann","pin":"1234","tenant":"22222222-2222-2222-2222-222222222222"}"#,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["roles"], json!(["staff"]));
    assert_eq!(v["data"]["expires_at"], 1600);
    let token = v["data"]["token"].as_str().unwrap();
    // Any route that needs a token accepts it; before the token it is a 401.
    let barcode = format!("/v1/{INST}/_barcode?type=code128&data=A1");
    let (s, v) = call(&app, "GET", &barcode, Some(token), "").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = call(&app, "GET", &barcode, None, "").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Staff may not write.
    let (s, _) = call(&app, "POST", &format!("/v1/{INST}/item"), Some(token), "{}").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bad_credentials_and_unusable_roles_look_the_same() {
    let app = build(true);
    let (s1, v1) = login(&app, r#"{"user":"ann","pin":"0000"}"#).await;
    let (s2, v2) = login(&app, r#"{"user":"nobody","pin":"1"}"#).await;
    let (s3, v3) = login(&app, r#"{"user":"bo","pin":"4321"}"#).await;
    for (s, v) in [(&s1, &v1), (&s2, &v2), (&s3, &v3)] {
        assert_eq!(*s, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["code"], "invalid_credentials", "{v}");
        assert_eq!(v["detail"], v1["detail"]);
    }
}

#[tokio::test]
async fn throttling_unavailability_and_misuse_have_their_own_answers() {
    let app = build(true);
    let (s, v) = login(&app, r#"{"user":"down"}"#).await;
    assert_eq!(
        (s, v["retryable"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!(true))
    );
    for _ in 0..3 {
        login(&app, r#"{"user":"ann","pin":"x"}"#).await;
    }
    // Locked out even with the right PIN.
    let (s, v) = login(&app, r#"{"user":"ann","pin":"1234"}"#).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["retryable"], true);

    let app = build(true);
    let (s, _) = login(&app, "[1]").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let big = format!(r#"{{"pin":"{}"}}"#, "9".repeat(5000));
    let (s, _) = login(&app, &big).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    let (s, _) = call(&app, "POST", "/v1/other/_auth/token", None, "{}").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_a_login_the_endpoint_is_unavailable_and_bad_setup_is_refused() {
    let app = build(false);
    let (s, _) = login(&app, r#"{"user":"ann","pin":"1234"}"#).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let m = validate(
        std::fs::read_to_string(format!(
            "{}/../../spec/examples/inventory.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
        .as_bytes(),
    )
    .unwrap();
    let st = || {
        AppState::new(
            MemoryStore::new(),
            Verifier::new(KEY).unwrap(),
            m.clone(),
            INST,
        )
    };
    let pins = || {
        Box::new(Pins {
            wrong: AtomicU32::new(0),
        })
    };
    assert!(st().with_login(pins(), "NOT-A-UUID", 60).is_err());
    assert!(st().with_login(pins(), T1, 0).is_err());
    assert!(st().with_login(pins(), T1, 86_401).is_err());
}
