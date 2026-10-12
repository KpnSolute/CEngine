//! The staff PIN login: legacy hashes, lockout, role ladder and the end-to-end token.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{hash_pin, router, AppState, Login, LoginError, PinLogin, PinPolicy};
use ce_store::MemoryStore;
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

/// Made by the legacy Python `hash_staff_pin` for the PIN 482915.
const LEGACY: &str = "kpn-scrypt$1$16384$8$1$a1b2c3d4e5f60718293a4b5c6d7e8f90$05bd019bf1b1e7d9b567cbd6fad27394d423c8688223c7ad5afc6159c52d9b34";
const T1: &str = "11111111-1111-1111-1111-111111111111";

fn creds(u: &str, p: &str) -> Map<String, Value> {
    json!({"username": u, "pin": p})
        .as_object()
        .unwrap()
        .clone()
}

fn staff_list(extra: &str) -> String {
    let own = hash_pin("7391", b"saltsalt12345678").unwrap();
    format!(
        r#"{{"users":[
            {{"username":"Cook","pin_hash":"{LEGACY}","roles":["staff"]}},
            {{"username":"chef","pin_hash":"{own}","roles":["manager"]}}{extra}]}}"#
    )
}

fn login_at(clock: Arc<AtomicU64>, text: &str) -> PinLogin {
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = {
        let c = clock.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    };
    PinLogin::from_json(text, PinPolicy::default(), now).unwrap()
}

#[test]
fn a_legacy_hash_verifies_and_the_username_ignores_case() {
    let l = login_at(Arc::new(AtomicU64::new(1000)), &staff_list(""));
    let who = l.login(&creds("  COOK ", "482915")).unwrap();
    assert_eq!(who.sub, "cook");
    assert_eq!(who.roles, vec!["staff"]);
    assert_eq!(l.login(&creds("cook", "482916")), Err(LoginError::Denied));
    let who = l.login(&creds("chef", "7391")).unwrap();
    assert_eq!(who.roles, vec!["staff", "assistant", "manager"]);
}

#[test]
fn five_wrong_pins_lock_for_fifteen_minutes_even_for_the_right_pin() {
    let clock = Arc::new(AtomicU64::new(1000));
    let l = login_at(clock.clone(), &staff_list(""));
    for _ in 0..5 {
        assert_eq!(l.login(&creds("cook", "0")), Err(LoginError::Denied));
    }
    assert_eq!(
        l.login(&creds("cook", "482915")),
        Err(LoginError::Throttled)
    );
    // Other staff are not locked.
    assert!(l.login(&creds("chef", "7391")).is_ok());
    clock.store(1000 + 899, Ordering::SeqCst);
    assert_eq!(
        l.login(&creds("cook", "482915")),
        Err(LoginError::Throttled)
    );
    clock.store(1000 + 900, Ordering::SeqCst);
    assert!(l.login(&creds("cook", "482915")).is_ok());
}

#[test]
fn a_good_pin_clears_the_count_and_unknown_names_lock_too() {
    let l = login_at(Arc::new(AtomicU64::new(1000)), &staff_list(""));
    for _ in 0..4 {
        l.login(&creds("cook", "0")).unwrap_err();
    }
    assert!(l.login(&creds("cook", "482915")).is_ok());
    for _ in 0..4 {
        l.login(&creds("cook", "0")).unwrap_err();
    }
    assert_eq!(l.login(&creds("cook", "0")), Err(LoginError::Denied));
    assert_eq!(
        l.login(&creds("cook", "482915")),
        Err(LoginError::Throttled)
    );
    // Probing a name that does not exist is counted the same way.
    for _ in 0..5 {
        assert_eq!(l.login(&creds("ghost", "1")), Err(LoginError::Denied));
    }
    assert_eq!(l.login(&creds("ghost", "1")), Err(LoginError::Throttled));
}

#[test]
fn missing_or_oversized_credentials_are_denied_without_counting() {
    let l = login_at(Arc::new(AtomicU64::new(1000)), &staff_list(""));
    for _ in 0..10 {
        assert_eq!(l.login(&creds("cook", "")), Err(LoginError::Denied));
        assert_eq!(l.login(&creds("", "1")), Err(LoginError::Denied));
        assert_eq!(
            l.login(&creds("cook", &"9".repeat(65))),
            Err(LoginError::Denied)
        );
    }
    assert!(l.login(&creds("cook", "482915")).is_ok());
}

#[test]
fn a_bad_staff_list_is_refused() {
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 0);
    let try_it = |t: &str| PinLogin::from_json(t, PinPolicy::default(), now.clone()).err();
    assert!(try_it("nope").is_some());
    assert!(
        try_it(r#"{"users":[{"username":"a","pin_hash":"plain","roles":["staff"]}]}"#).is_some()
    );
    let greedy = LEGACY.replace("$16384$", "$1048576$");
    assert!(try_it(&format!(
        r#"{{"users":[{{"username":"a","pin_hash":"{greedy}","roles":["staff"]}}]}}"#
    ))
    .is_some());
    let bad_role =
        format!(r#"{{"users":[{{"username":"a","pin_hash":"{LEGACY}","roles":["owner"]}}]}}"#);
    assert!(try_it(&bad_role).is_some());
    let dup = format!(
        r#"{{"users":[{{"username":"a","pin_hash":"{LEGACY}","roles":["staff"]}},{{"username":"A","pin_hash":"{LEGACY}","roles":["staff"]}}]}}"#
    );
    assert!(try_it(&dup).is_some());
    assert!(hash_pin("", b"saltsalt").is_none());
}

#[tokio::test]
async fn a_pin_login_gets_a_twelve_hour_token_that_the_server_accepts() {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let m = validate(v.to_string().as_bytes()).unwrap();
    let key = b"0123456789abcdef0123456789abcdef";
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(key).unwrap(), m, "pin-1");
    st.now = Arc::new(|| 1000);
    let login = login_at(Arc::new(AtomicU64::new(1000)), &staff_list(""));
    let app = router(st.with_login(Box::new(login), T1, 12 * 3600).unwrap());
    let post = |body: &str| {
        Request::builder()
            .method("POST")
            .uri("/v1/pin-1/_auth/token")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let res = app
        .clone()
        .oneshot(post(r#"{"username":"chef","pin":"7391"}"#))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let b: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(b["data"]["expires_at"], 1000 + 43_200);
    assert_eq!(b["data"]["roles"], json!(["staff", "manager"]));
    let token = b["data"]["token"].as_str().unwrap();
    let req = Request::builder()
        .uri("/v1/pin-1/_barcode?type=code128&data=A1")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    let res = app
        .oneshot(post(r#"{"username":"chef","pin":"1"}"#))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
