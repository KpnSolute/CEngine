//! Staff administration over HTTP: create, rotate, reset, switch off, and who may do what.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{
    hash_pin, router, weak_pin, AppState, MemoryDirectory, PinLogin, PinPolicy, StaffRecord,
};
use ce_store::MemoryStore;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

const T1: &str = "11111111-1111-1111-1111-111111111111";

fn rec(name: &str, pin: &str, role: &str) -> StaffRecord {
    StaffRecord {
        username: name.into(),
        hash: hash_pin(pin, b"saltsalt12345678").unwrap(),
        roles: vec![role.into()],
        active: true,
        must_rotate: false,
        pin_updated_at: 0,
    }
}

fn app(writable: bool) -> Router {
    app_at(writable, Arc::new(std::sync::atomic::AtomicU64::new(1000)))
}

fn app_at(writable: bool, clock: Arc<std::sync::atomic::AtomicU64>) -> Router {
    use std::sync::atomic::Ordering::SeqCst;
    let m = validate(
        json!({
            "manifest_version": 1,
            "template": {"name": "t", "version": "0.1.0"},
            "entities": {"note": {"fields": {"text": {"type": "string", "max_length": 40}}}},
            "roles": {
                "staff": {"read": ["note"], "write": [], "actions": []},
                "manager": {"read": ["*"], "write": ["*"], "actions": []},
                "admin": {"read": ["*"], "write": ["*"], "actions": []},
                "sudo": {"read": ["*"], "write": ["*"], "actions": []}
            }
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let dir = MemoryDirectory::new(
        vec![
            rec("boss", "739142", "admin"),
            rec("root", "551827", "sudo"),
            rec("mgr", "624801", "manager"),
        ],
        writable,
    );
    let login = PinLogin::with_directory(Arc::new(dir), PinPolicy::default(), {
        let c = clock.clone();
        Arc::new(move || c.load(SeqCst))
    });
    let mut st = AppState::new(
        MemoryStore::new(),
        Verifier::new(b"0123456789abcdef0123456789abcdef").unwrap(),
        m,
        "s",
    );
    st.now = Arc::new(move || clock.load(SeqCst));
    router(st.with_login(Box::new(login), T1, 3600).unwrap())
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut r = Request::builder()
        .method(method)
        .uri(format!("/v1/s{path}"));
    if let Some(t) = token {
        r = r.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = app
        .clone()
        .oneshot(r.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let st = res.status();
    let b = res.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn sign_in(app: &Router, user: &str, pin: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/_auth/token",
        None,
        json!({"username": user, "pin": pin}),
    )
    .await
}

async fn token(app: &Router, user: &str, pin: &str) -> String {
    let (st, b) = sign_in(app, user, pin).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    b["data"]["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_admin_creates_staff_who_must_pick_their_own_pin() {
    let a = app(true);
    let boss = token(&a, "boss", "739142").await;
    let (st, b) = call(
        &a,
        "POST",
        "/_staff",
        Some(&boss),
        json!({"username": "Newbie", "roles": ["staff"], "pin": "482915"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["data"]["username"], "newbie");
    assert_eq!(b["data"]["must_rotate"], true);
    assert!(!b.to_string().contains("kpn-scrypt"));

    let (st, b) = sign_in(&a, "newbie", "482915").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["data"]["must_rotate"], true);
    assert_eq!(b["data"]["roles"], json!(["_rotate_pin"]));
    let tmp = b["data"]["token"].as_str().unwrap().to_string();
    // The forced-change token opens nothing else.
    assert_eq!(
        call(&a, "GET", "/note", Some(&tmp), Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&a, "GET", "/_staff", Some(&tmp), Value::Null).await.0,
        StatusCode::FORBIDDEN
    );

    let change = |cur: &str, new: &str| json!({"current_pin": cur, "new_pin": new});
    assert_eq!(
        call(
            &a,
            "POST",
            "/_auth/pin",
            Some(&tmp),
            change("000000", "846213")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for weak in ["123456", "111111", "4821", "482915"] {
        let (st, _) = call(&a, "POST", "/_auth/pin", Some(&tmp), change("482915", weak)).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{weak}");
    }
    let (st, b) = call(
        &a,
        "POST",
        "/_auth/pin",
        Some(&tmp),
        change("482915", "846213"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");

    assert_eq!(
        sign_in(&a, "newbie", "482915").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (st, b) = sign_in(&a, "newbie", "846213").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["data"]["must_rotate"], false);
    assert_eq!(b["data"]["roles"], json!(["staff"]));
}

#[tokio::test]
async fn who_may_do_what() {
    let a = app(true);
    let boss = token(&a, "boss", "739142").await;
    let mgr = token(&a, "mgr", "624801").await;
    // Only the admin role administers staff.
    assert_eq!(
        call(&a, "GET", "/_staff", Some(&mgr), Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&a, "GET", "/_staff", None, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (st, b) = call(&a, "GET", "/_staff", Some(&boss), Value::Null).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["data"].as_array().unwrap().len(), 3);
    assert!(!b.to_string().contains("kpn-scrypt"));

    let make = |name: &str, role: &str| json!({"username": name, "roles": [role], "pin": "482915"});
    // An admin cannot make a sudo, nor invent a role, nor take a taken name.
    assert_eq!(
        call(&a, "POST", "/_staff", Some(&boss), make("x", "sudo"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&a, "POST", "/_staff", Some(&boss), make("x", "janitor"))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&a, "POST", "/_staff", Some(&boss), make("MGR", "staff"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &a,
            "POST",
            "/_staff",
            Some(&boss),
            json!({"username": "x", "roles": ["staff"], "pin": "123456"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Nor touch a sudo account, nor themselves.
    assert_eq!(
        call(
            &a,
            "PATCH",
            "/_staff/root",
            Some(&boss),
            json!({"active": false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &a,
            "POST",
            "/_staff/root/reset_pin",
            Some(&boss),
            json!({"pin": "739142"})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &a,
            "PATCH",
            "/_staff/boss",
            Some(&boss),
            json!({"active": false})
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &a,
            "PATCH",
            "/_staff/nobody",
            Some(&boss),
            json!({"active": false})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // A sudo can.
    let root = token(&a, "root", "551827").await;
    assert_eq!(
        call(&a, "POST", "/_staff", Some(&root), make("x", "sudo"))
            .await
            .0,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn switching_off_and_resetting() {
    let a = app(true);
    let boss = token(&a, "boss", "739142").await;
    let (st, b) = call(
        &a,
        "PATCH",
        "/_staff/mgr",
        Some(&boss),
        json!({"active": false}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(
        sign_in(&a, "mgr", "624801").await.0,
        StatusCode::UNAUTHORIZED
    );
    call(
        &a,
        "PATCH",
        "/_staff/mgr",
        Some(&boss),
        json!({"active": true, "roles": ["staff"]}),
    )
    .await;
    let (st, b) = sign_in(&a, "mgr", "624801").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["data"]["roles"], json!(["staff"]));

    let (st, b) = call(
        &a,
        "POST",
        "/_staff/mgr/reset_pin",
        Some(&boss),
        json!({"pin": "913574"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["data"]["must_rotate"], true);
    assert_eq!(
        sign_in(&a, "mgr", "624801").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, b) = sign_in(&a, "mgr", "913574").await;
    assert_eq!(b["data"]["must_rotate"], true);
}

#[tokio::test]
async fn a_staff_file_installation_is_read_only() {
    let a = app(false);
    let boss = token(&a, "boss", "739142").await;
    let (st, b) = call(
        &a,
        "POST",
        "/_staff",
        Some(&boss),
        json!({"username": "x", "roles": ["staff"], "pin": "482915"}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(b["code"], "read_only");
    let (st, _) = call(
        &a,
        "POST",
        "/_auth/pin",
        Some(&boss),
        json!({"current_pin": "739142", "new_pin": "846213"}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[test]
fn weak_pins() {
    for w in [
        "", "1234", "123456", "000000", "111111", "98765", "7777777", "456789",
    ] {
        assert!(weak_pin(w, 6), "{w}");
    }
    for ok in ["482915", "739142", "1357902468"] {
        assert!(!weak_pin(ok, 6), "{ok}");
    }
}

#[tokio::test]
async fn a_forced_change_token_opens_nothing_else() {
    let a = app(true);
    let boss = token(&a, "boss", "739142").await;
    call(
        &a,
        "POST",
        "/_staff",
        Some(&boss),
        json!({"username": "n", "roles": ["staff"], "pin": "482915"}),
    )
    .await;
    let (_, b) = sign_in(&a, "n", "482915").await;
    let tmp = b["data"]["token"].as_str().unwrap().to_string();
    for path in ["/_meta/manifest", "/_meta/openapi", "/v1/s/note"] {
        let res = a
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {tmp}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "{path}");
    }
    // The same routes accept a normal token.
    let ok = token(&a, "boss", "739142").await;
    let res = a
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_meta/manifest")
                .header(header::AUTHORIZATION, format!("Bearer {ok}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_admin_token_for_another_tenant_cannot_administer_staff() {
    let a = app(true);
    let v = Verifier::new(b"0123456789abcdef0123456789abcdef").unwrap();
    let other = v
        .sign_token(
            "boss",
            "22222222-2222-2222-2222-222222222222",
            &["admin".to_string()],
            1000,
            3600,
        )
        .unwrap();
    assert_eq!(
        call(&a, "GET", "/_staff", Some(&other), Value::Null)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (st, _) = call(
        &a,
        "POST",
        "/_auth/pin",
        Some(&other),
        json!({"current_pin": "739142", "new_pin": "846213"}),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// A directory that fails its first lookups, then works.
struct Flaky {
    inner: MemoryDirectory,
    failures: std::sync::atomic::AtomicU32,
    list_failures: std::sync::atomic::AtomicU32,
}
impl ce_server::StaffDirectory for Flaky {
    fn get(&self, u: &str) -> Result<Option<StaffRecord>, ce_server::DirectoryFailure> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.failures.load(SeqCst) > 0 {
            self.failures.fetch_sub(1, SeqCst);
            return Err(ce_server::DirectoryFailure::Backend);
        }
        self.inner.get(u)
    }
    fn list(&self) -> Result<Vec<StaffRecord>, ce_server::DirectoryFailure> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.list_failures.load(SeqCst) > 0 {
            self.list_failures.fetch_sub(1, SeqCst);
            return Err(ce_server::DirectoryFailure::Backend);
        }
        self.inner.list()
    }
    fn create(&self, r: StaffRecord, a: &str) -> Result<(), ce_server::DirectoryFailure> {
        self.inner.create(r, a)
    }
    fn set_pin(
        &self,
        u: &str,
        h: &str,
        m: bool,
        n: u64,
        a: &str,
    ) -> Result<(), ce_server::DirectoryFailure> {
        self.inner.set_pin(u, h, m, n, a)
    }
    fn set_roles(&self, u: &str, r: &[String], a: &str) -> Result<(), ce_server::DirectoryFailure> {
        self.inner.set_roles(u, r, a)
    }
    fn set_active(&self, u: &str, v: bool, a: &str) -> Result<(), ce_server::DirectoryFailure> {
        self.inner.set_active(u, v, a)
    }
    fn writable(&self) -> bool {
        true
    }
}

#[test]
fn database_errors_do_not_count_as_wrong_pins() {
    use ce_server::{Login, LoginError};
    let dir = Flaky {
        inner: MemoryDirectory::new(vec![rec("boss", "739142", "admin")], true),
        failures: std::sync::atomic::AtomicU32::new(10),
        list_failures: std::sync::atomic::AtomicU32::new(0),
    };
    let login = PinLogin::with_directory(Arc::new(dir), PinPolicy::default(), Arc::new(|| 1000));
    let creds = json!({"username": "boss", "pin": "739142"})
        .as_object()
        .unwrap()
        .clone();
    for _ in 0..10 {
        assert_eq!(login.login(&creds), Err(LoginError::Unavailable));
    }
    assert!(login.login(&creds).is_ok());
}

#[tokio::test]
async fn a_reset_a_pin_change_or_a_switch_off_ends_earlier_sessions() {
    use std::sync::atomic::Ordering::SeqCst;
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(1000));
    let a = app_at(true, clock.clone());
    let boss = token(&a, "boss", "739142").await;
    let mgr = token(&a, "mgr", "624801").await;
    let live = |t: String| {
        let a = a.clone();
        async move {
            call(&a, "GET", "/note", Some(&t), Value::Null).await.0 != StatusCode::UNAUTHORIZED
        }
    };
    assert!(live(mgr.clone()).await);

    // Switching the account off ends its session at once, and switching it back on does not
    // bring the old token back.
    clock.store(1100, SeqCst);
    call(
        &a,
        "PATCH",
        "/_staff/mgr",
        Some(&boss),
        json!({"active": false}),
    )
    .await;
    assert!(!live(mgr.clone()).await);
    clock.store(1200, SeqCst);
    call(
        &a,
        "PATCH",
        "/_staff/mgr",
        Some(&boss),
        json!({"active": true}),
    )
    .await;
    assert!(!live(mgr.clone()).await);
    let mgr2 = token(&a, "mgr", "624801").await;
    assert!(live(mgr2.clone()).await);

    // A reset ends the session; so does the person's own PIN change.
    clock.store(1300, SeqCst);
    call(
        &a,
        "POST",
        "/_staff/mgr/reset_pin",
        Some(&boss),
        json!({"pin": "913574"}),
    )
    .await;
    assert!(!live(mgr2).await);
    clock.store(1400, SeqCst);
    let rot = token(&a, "mgr", "913574").await;
    let (st, b) = call(
        &a,
        "POST",
        "/_auth/pin",
        Some(&rot),
        json!({"current_pin": "913574", "new_pin": "482916"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    clock.store(1500, SeqCst);
    let mgr3 = token(&a, "mgr", "482916").await;
    assert!(live(mgr3).await);
    // Other people's sessions are untouched.
    assert!(live(boss).await);
}

#[test]
fn tokens_are_refused_until_the_revocation_state_has_loaded() {
    use ce_server::Login;
    let dir = Flaky {
        inner: MemoryDirectory::new(vec![rec("boss", "739142", "admin")], true),
        failures: std::sync::atomic::AtomicU32::new(0),
        list_failures: std::sync::atomic::AtomicU32::new(1),
    };
    let login = PinLogin::with_directory(Arc::new(dir), PinPolicy::default(), Arc::new(|| 1000));
    // The directory could not be read at start: every token is refused, not accepted unchecked.
    assert_eq!(login.not_before("boss"), Some(u64::MAX));
    let creds = json!({"username": "boss", "pin": "739142"})
        .as_object()
        .unwrap()
        .clone();
    assert!(login.login(&creds).is_ok());
    assert_eq!(login.not_before("boss"), None);
}

#[tokio::test]
async fn revocation_covers_the_same_second_and_only_this_tenant() {
    let a = app(true);
    let boss = token(&a, "boss", "739142").await;
    let mgr = token(&a, "mgr", "624801").await;
    let live = |t: String| {
        let a = a.clone();
        async move {
            call(&a, "GET", "/note", Some(&t), Value::Null).await.0 != StatusCode::UNAUTHORIZED
        }
    };
    // A token for another tenant that shares the username.
    const T2: &str = "22222222-2222-2222-2222-222222222222";
    let other = Verifier::new(b"0123456789abcdef0123456789abcdef")
        .unwrap()
        .sign_token("mgr", T2, &["manager".to_string()], 1000, 3600)
        .unwrap();
    assert!(live(mgr.clone()).await && live(other.clone()).await);
    // Same clock second as the token: the reset still ends it.
    let (st, _) = call(
        &a,
        "POST",
        "/_staff/mgr/reset_pin",
        Some(&boss),
        json!({"pin": "913574"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!live(mgr).await);
    assert!(live(other).await);
    // Signing in again in that same second gives a token that works.
    let fresh = token(&a, "mgr", "913574").await;
    let (st, _) = call(&a, "GET", "/note", Some(&fresh), Value::Null).await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a rotation-only token, but accepted as valid"
    );
    // A second revocation in the same second reaches the token dated just after the first.
    assert!(live(fresh.clone()).await);
    let (st, _) = call(
        &a,
        "POST",
        "/_staff/mgr/reset_pin",
        Some(&boss),
        json!({"pin": "802468"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !live(fresh).await,
        "a token issued in the same second as the earlier revocation survived the next one"
    );
}
