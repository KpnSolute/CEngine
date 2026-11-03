//! Guest accounts end to end: sign-up and sign-in, rows only the guest owns, nothing a guest sends can
//! set state or owner, guest and staff tokens open different doors, and a role-restricted projection.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, AppState, GuestPolicy, GuestService, MemoryGuests};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "gu-1";
const NOW: u64 = 1_793_664_000; // 2026-11-03

fn staff_token(role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{role}-user","tenant":"{T1}","roles":["{role}"],"exp":{}}}"#,
        NOW + 10_000
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn manifest() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "guests", "title": "Guests", "version": "0.1.0"},
        "entities": {
            "log": {"fields": {
                "entry_type": {"type": "string", "max_length": 60, "required": true, "indexed": true},
                "title": {"type": "string", "max_length": 200},
                "business_date": {"type": "date", "indexed": true},
                "created_by": {"type": "string", "max_length": 120, "indexed": true},
                "data": {"type": "json"}
            }, "sort_keys": ["business_date"]},
            "setting": {"fields": {
                "setting_key": {"type": "string", "max_length": 80, "required": true, "unique": true},
                "setting_value": {"type": "json", "required": true}
            }, "sort_keys": ["setting_key"]}
        },
        "roles": {
            "staff": {"read": ["log", "setting"], "write": ["log", "setting"]},
            "cook": {"read": ["log"]},
            "guest": {}
        },
        "projections": {
            "access": {"entity": "setting", "fields": ["setting_value"], "filter": "setting_key == 'hub_access'", "roles": ["staff", "cook"]}
        },
        "guest": {
            "role": "guest",
            "resources": {"requests": {
                "entity": "log", "owner_field": "created_by",
                "fixed": {"entry_type": "food_request"},
                "stamp": {"business_date": "today"},
                "created_at_field": "business_date",
                "sort": "-business_date",
                "title_field": "title", "title": "concat('Food request: ', dept)",
                "body": {
                    "field": "data", "as": "form",
                    "allow": ["originator", "dept", "eventDate", "location", "food"],
                    "required": ["originator", "dept", "eventDate", "location"],
                    "rules": [
                        {"expr": "event_date >= today", "field": "eventDate", "message": "The event date has passed."},
                        {"expr": "food != ''", "field": "food", "message": "Ask for something."}
                    ],
                    "state": {"key": "status", "initial": "submitted"},
                    "note_key": "note"
                }
            }},
            "feeds": {"catering": {
                "entity": "log", "fixed": {"entry_type": "food_request"}, "body_field": "data",
                "keys": ["status", "eventDate", "theme", "dept", "location"],
                "where": "event_date >= today and coalesce(status, 'submitted') != 'declined'",
                "fields": {
                    "title": {"expr": "when(theme != '', theme, dept)"},
                    "eventDate": {"from": "eventDate"}, "location": {"from": "location"},
                    "status": {"expr": "coalesce(status, 'submitted')"}
                },
                "sort": "-business_date", "order_by": "eventDate"
            }},
            "public": {"survey": {"entity": "setting", "match": {"field": "setting_key", "value": "hub_survey_url"}, "value_field": "setting_value"}}
        }
    })
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

fn build(url: &str, clock: Arc<AtomicU64>) -> Router {
    build_with(url, clock, None)
}

struct Outbox(std::sync::Mutex<Vec<(String, String)>>);
impl ce_server::Mailer for Outbox {
    fn send_reset(&self, to: &str, _name: &str, link: &str) -> Result<(), ce_server::MailError> {
        self.0
            .lock()
            .unwrap()
            .push((to.to_string(), link.to_string()));
        Ok(())
    }
}

fn build_with(
    url: &str,
    clock: Arc<AtomicU64>,
    mailer: Option<Arc<dyn ce_server::Mailer>>,
) -> Router {
    let m = validate(manifest().to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_gu_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let c = clock.clone();
    let now: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(move || c.load(Ordering::SeqCst));
    let policy = GuestPolicy {
        reset_link_base: "https://hub.example/hub/guest/reset".into(),
        ..GuestPolicy::default()
    };
    let svc = GuestService::new(
        Arc::new(MemoryGuests::default()),
        mailer,
        policy,
        now.clone(),
    )
    .unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST)
        .with_guests(svc, T1)
        .unwrap();
    st.now = now;
    router(st)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let res = app
        .clone()
        .oneshot(
            req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
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

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn g(path: &str) -> String {
    format!("/v1/{INST}/_guest/{path}")
}

fn form(extra: Value) -> Value {
    let mut f = json!({"originator": "Jo", "dept": "Wellness", "eventDate": "2026-12-10", "location": "Room 4", "food": "Pizza"});
    f.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    json!({"form": f})
}

async fn signup(app: &Router, email: &str) -> String {
    let (s, b) = call(
        app,
        "POST",
        &g("signup"),
        None,
        Some(json!({"name": "Guest", "email": email, "password": "correct horse"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b["data"]["token"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_resources_and_doors() {
    let Some(url) = url() else { return };
    let clock = Arc::new(AtomicU64::new(NOW));
    let c2 = clock.clone();
    let app = tokio::task::spawn_blocking(move || build(&url, c2))
        .await
        .unwrap();

    // Sign up, duplicate, wrong password, right password.
    let tok = signup(&app, "jo@example.com").await;
    let (s, _) = call(
        &app,
        "POST",
        &g("signup"),
        None,
        Some(json!({"name": "Jo", "email": "JO@example.com", "password": "another one 1"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, b) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "jo@example.com", "password": "wrong password"})),
        &[],
    )
    .await;
    assert_eq!(
        (s, b["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("invalid_credentials"))
    );
    let (s, b) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "jo@example.com", "password": "correct horse"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["data"]["name"], "Guest");
    assert!(b["data"]["expires_at"].as_u64().unwrap() > NOW);

    // A guest sends a request and tries to set the state, the note and the owner.
    let sent =
        form(json!({"status": "approved", "note": "free food", "created_by": "guest:other"}));
    let (s, b) = call(&app, "POST", &g("requests"), Some(&tok), Some(sent), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["data"]["status"], "submitted");
    assert_eq!(b["data"]["created_at"], "2026-11-03");
    assert!(b["data"]["note"].is_null());
    let id = b["data"]["id"].as_str().unwrap().to_string();

    // Any signed-in guest sees the event, not who asked or what they ordered; nobody else gets in.
    let (s, b) = call(&app, "GET", &g("catering"), Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["data"],
        json!([{"id": id, "title": "Wellness", "eventDate": "2026-12-10", "location": "Room 4", "status": "submitted"}])
    );
    let (s, _) = call(&app, "GET", &g("catering"), None, None, &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(
        &app,
        "GET",
        &g("catering"),
        Some(&staff_token("staff")),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Validation reads like the staff form.
    let (s, b) = call(
        &app,
        "POST",
        &g("requests"),
        Some(&tok),
        Some(form(json!({"eventDate": "2026-11-01"}))),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(b["errors"]
        .to_string()
        .contains("The event date has passed."));
    let (s, _) = call(
        &app,
        "POST",
        &g("requests"),
        Some(&tok),
        Some(form(json!({"food": ""}))),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(
        &app,
        "POST",
        &g("requests"),
        Some(&tok),
        Some(json!({"form": "nope"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Staff see the row as the old app stored it, owned by the guest, and decide it.
    let staff = staff_token("staff");
    let (s, b) = call(
        &app,
        "GET",
        &format!(
            "/v1/{INST}/log?filter={}",
            enc(&json!({"field": "entry_type", "op": "eq", "value": "food_request"}).to_string())
        ),
        Some(&staff),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = &b["data"][0];
    assert_eq!(row["id"], id.as_str());
    assert!(row["created_by"].as_str().unwrap().starts_with("guest:"));
    assert_eq!(row["data"]["status"], "submitted");
    assert_eq!(row["title"], "Food request: Wellness");
    let mut data = row["data"].clone();
    data["status"] = json!("declined");
    data["note"] = json!("Kitchen is closed that day");
    data["decidedBy"] = json!("staff-user");
    let version = row["version"].as_u64().unwrap().to_string();
    let (s, b) = call(
        &app,
        "PATCH",
        &format!("/v1/{INST}/log/{id}"),
        Some(&staff),
        Some(json!({"data": data})),
        &[("if-match", &format!("\"{version}\""))],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let (_, b) = call(&app, "GET", &g("catering"), Some(&tok), None, &[]).await;
    assert_eq!(
        b["data"],
        json!([]),
        "a declined request is not on the catering calendar"
    );

    // The guest sees the decision and the note, never who decided.
    let (s, b) = call(&app, "GET", &g("requests"), Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["data"].as_array().unwrap().len(), 1);
    assert_eq!(b["data"][0]["status"], "declined");
    assert_eq!(b["data"][0]["note"], "Kitchen is closed that day");
    assert!(b["data"][0]["form"].get("decidedBy").is_none());

    // A second guest sees none of it; a staff-made food_request is not theirs either.
    let tok2 = signup(&app, "al@example.com").await;
    let (_, b) = call(&app, "GET", &g("requests"), Some(&tok2), None, &[]).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 0);
    let (s, _) = call(&app, "POST", &format!("/v1/{INST}/log"), Some(&staff), Some(json!({"entry_type": "food_request", "title": "staff made", "created_by": "staff-user", "data": {"status": "approved", "eventDate": "2027-01-05", "dept": "Culinary", "location": "Hall", "food": "private"}})), &[("idempotency-key", "s1")]).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, b) = call(&app, "GET", &g("catering"), Some(&tok2), None, &[]).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 1);
    assert_eq!(b["data"][0]["status"], "approved");
    assert_eq!(b["data"][0]["title"], "Culinary");
    assert!(
        !b.to_string().contains("private")
            && !b.to_string().contains("staff-user")
            && !b.to_string().contains("created_by")
    );
    let (_, b) = call(&app, "GET", &g("requests"), Some(&tok2), None, &[]).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 0);
    let (_, b) = call(&app, "GET", &g("requests"), Some(&tok), None, &[]).await;
    assert_eq!(b["data"].as_array().unwrap().len(), 1);

    // Different doors: a guest token opens no staff route; a staff token opens no guest route.
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/{INST}/log"),
        Some(&tok),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/{INST}/log"),
        Some(&tok),
        Some(json!({"entry_type": "x"})),
        &[("idempotency-key", "g1")],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, "GET", &g("requests"), Some(&staff), None, &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, "GET", &g("requests"), None, None, &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, "GET", &g("nothing"), Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // The public survey link, then a role-restricted projection.
    let (s, b) = call(&app, "GET", &g("survey"), None, None, &[]).await;
    assert_eq!((s, &b["data"]), (StatusCode::OK, &json!({})));
    for (k, v) in [
        ("hub_survey_url", json!({"url": "https://forms.example/s"})),
        ("hub_access", json!({"cook": ["pos"]})),
    ] {
        let (s, b) = call(
            &app,
            "POST",
            &format!("/v1/{INST}/setting"),
            Some(&staff),
            Some(json!({"setting_key": k, "setting_value": v})),
            &[("idempotency-key", k)],
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
    }
    let (_, b) = call(&app, "GET", &g("survey"), None, None, &[]).await;
    assert_eq!(b["data"]["url"], "https://forms.example/s");
    let view = format!("/v1/{INST}/_view/access");
    let (s, b) = call(&app, "GET", &view, Some(&staff_token("cook")), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["data"][0]["setting_value"], json!({"cook": ["pos"]}));
    // The cook still cannot read settings directly, and a guest or an unknown role cannot use the view.
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/{INST}/setting"),
        Some(&staff_token("cook")),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, "GET", &view, Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(&app, "GET", &view, None, None, &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Sign out ends that guest's old tokens; signing in again straight after still works.
    let (s, _) = call(&app, "POST", &g("logout"), Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "GET", &g("requests"), Some(&tok), None, &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, b) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "jo@example.com", "password": "correct horse"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let fresh = b["data"]["token"].as_str().unwrap().to_string();
    let (s, _) = call(&app, "GET", &g("requests"), Some(&fresh), None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "GET", &g("requests"), Some(&tok2), None, &[]).await;
    assert_eq!(s, StatusCode::OK); // the other guest was not signed out

    tokio::task::spawn_blocking(move || drop(app))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_routes_answer_404_when_guests_are_off() {
    let Some(url) = url() else { return };
    let app = tokio::task::spawn_blocking(move || {
        let m = validate(manifest().to_string().as_bytes()).unwrap();
        let schema = format!(
            "ce_gu_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        router(AppState::new(store, Verifier::new(KEY).unwrap(), m, INST))
    })
    .await
    .unwrap();
    let (s, _) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "a@b.co", "password": "x"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    tokio::task::spawn_blocking(move || drop(app))
        .await
        .unwrap();
}

#[test]
fn the_guest_role_must_hold_nothing_and_server_keys_cannot_be_guest_writable() {
    let mut m = manifest();
    m["roles"]["guest"] = json!({"read": ["log"]});
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "guest_role_privileged"), "{e:?}");

    let mut m = manifest();
    m["guest"]["resources"]["requests"]["body"]["allow"] = json!([
        "originator",
        "dept",
        "eventDate",
        "location",
        "food",
        "status"
    ]);
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(
        e.iter().any(|i| i.code == "guest_writable_server_key"),
        "{e:?}"
    );

    let mut m = manifest();
    m["guest"]["resources"]["login"] = m["guest"]["resources"]["requests"].clone();
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "reserved_guest_path"), "{e:?}");

    let mut m = manifest();
    m["guest"]["resources"]["requests"]["owner_field"] = json!("entry_type");
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "bad_owner_field"), "{e:?}");

    let mut m = manifest();
    m["guest"]["feeds"]["catering"]["where"] = json!("nope_key >= today");
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "unknown_identifier"), "{e:?}");

    let mut m = manifest();
    m["guest"]["feeds"]["catering"]["fields"]["title"] = json!({"from": "not_declared"});
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "unknown_key"), "{e:?}");

    let mut m = manifest();
    m["guest"]["feeds"]["requests"] = m["guest"]["feeds"]["catering"].clone();
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "duplicate_guest_path"), "{e:?}");

    let mut m = manifest();
    m["guest"]["feeds"]["catering"]["order_by"] = json!("nothing");
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "unknown_field"), "{e:?}");

    let mut m = manifest();
    m["projections"]["access"]["roles"] = json!(["ghost"]);
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "unknown_role"), "{e:?}");

    let mut m = manifest();
    m["guest"]["resources"]["requests"]["body"]["rules"][0]["expr"] = json!("nope_field >= today");
    let e = validate(m.to_string().as_bytes()).unwrap_err();
    assert!(e.iter().any(|i| i.code == "unknown_identifier"), "{e:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_reset_by_email_works_once_and_ends_old_sessions() {
    let Some(url) = url() else { return };
    let clock = Arc::new(AtomicU64::new(NOW));
    let (c2, outbox) = (
        clock.clone(),
        Arc::new(Outbox(std::sync::Mutex::new(vec![]))),
    );
    let ob: Arc<dyn ce_server::Mailer> = outbox.clone();
    let app = tokio::task::spawn_blocking(move || build_with(&url, c2, Some(ob)))
        .await
        .unwrap();
    let old = signup(&app, "jo@example.com").await;

    // The answer is the same for an account that exists and one that does not; mail goes only to the real one.
    for email in ["nobody@example.com", "jo@example.com"] {
        let (s, b) = call(
            &app,
            "POST",
            &g("reset"),
            None,
            Some(json!({"email": email})),
            &[],
        )
        .await;
        assert_eq!(
            (s, &b["data"]["sent"]),
            (StatusCode::ACCEPTED, &json!(true))
        );
    }
    let sent = outbox.0.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "jo@example.com");
    let token = sent[0].1.split("token=").nth(1).unwrap().to_string();
    assert!(sent[0]
        .1
        .starts_with("https://hub.example/hub/guest/reset?token="));

    clock.fetch_add(30, Ordering::SeqCst);
    let (s, _) = call(
        &app,
        "POST",
        &g("reset/confirm"),
        None,
        Some(json!({"token": token, "password": "short"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(
        &app,
        "POST",
        &g("reset/confirm"),
        None,
        Some(json!({"token": "wrong", "password": "brand new pass"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // The weak-password attempt used the token up (single use), so a fresh one is needed.
    let (s, _) = call(
        &app,
        "POST",
        &g("reset"),
        None,
        Some(json!({"email": "jo@example.com"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let token = outbox.0.lock().unwrap()[1]
        .1
        .split("token=")
        .nth(1)
        .unwrap()
        .to_string();
    let (s, _) = call(
        &app,
        "POST",
        &g("reset/confirm"),
        None,
        Some(json!({"token": token, "password": "brand new pass"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &app,
        "POST",
        &g("reset/confirm"),
        None,
        Some(json!({"token": token, "password": "brand new pass"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    let (s, _) = call(&app, "GET", &g("requests"), Some(&old), None, &[]).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "the old session ended with the reset"
    );
    let (s, _) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "jo@example.com", "password": "correct horse"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(
        &app,
        "POST",
        &g("login"),
        None,
        Some(json!({"email": "jo@example.com", "password": "brand new pass"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    tokio::task::spawn_blocking(move || drop(app))
        .await
        .unwrap();
}
