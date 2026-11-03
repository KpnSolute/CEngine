//! `emit_on_create`: a created record announces itself in the same transaction, once, and a
//! subscription can turn that into a record of its own.
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
const INST: &str = "eoc-1";
static N: AtomicU32 = AtomicU32::new(0);

fn token() -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{T1}","roles":["owner"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "eoc", "version": "0.1.0", "title": "Emit on create"},
            "entities": {
                "request": {"emit_on_create": "request.created", "fields": {
                    "title": {"type": "string", "max_length": 80, "required": true},
                    "who": {"type": "string", "max_length": 40}}},
                "plain": {"fields": {"title": {"type": "string", "max_length": 80}}},
                "note": {"fields": {
                    "text": {"type": "string", "max_length": 80},
                    "about": {"type": "string", "max_length": 80},
                    "to": {"type": "string", "max_length": 40}}}
            },
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}},
            "subscriptions": [{"event": "request.created", "create": {"entity": "note", "values": {
                "text": {"value": "A request came in"},
                "about": {"from": "subject"},
                "to": {"from": "data.record.who"}}}}]
        }),
        &["audit", "events"],
    )
    .unwrap()
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    key: &str,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", token()))
        .header("idempotency-key", key);
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
    let s = res.status();
    let b = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_created_record_announces_itself_once_and_a_subscription_reacts() {
    let Ok(url) = std::env::var("CE_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("CE_REQUIRE_DB").is_err(),
            "CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not"
        );
        eprintln!("skipping: CE_TEST_DATABASE_URL not set");
        return;
    };
    let app = tokio::task::spawn_blocking(move || {
        let m = validate(manifest().to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
        let schema = format!(
            "ce_eoc_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
        st.now = std::sync::Arc::new(|| 1000);
        router(st)
    })
    .await
    .unwrap();
    let list = |e: &'static str| {
        let app = app.clone();
        async move {
            call(&app, "GET", &format!("/v1/{INST}/{e}"), None, "r")
                .await
                .1["data"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        }
    };

    let (s, b) = call(
        &app,
        "POST",
        &format!("/v1/{INST}/request"),
        Some(json!({"title": "Lunch", "who": "sam"})),
        "a1",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["data"]["id"].as_str().unwrap().to_string();
    let evs = list("outbox_event").await;
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["name"], "request.created");
    assert_eq!(evs[0]["subject"], id.as_str());
    assert_eq!(evs[0]["data"]["record"]["title"], "Lunch");
    assert_eq!(evs[0]["data"]["actor"], "u1");
    let notes = list("note").await;
    assert_eq!(
        (
            notes.len(),
            notes[0]["about"].as_str(),
            notes[0]["to"].as_str()
        ),
        (1, Some(id.as_str()), Some("sam"))
    );

    // The same request sent again is a replay: still one event, one note.
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/{INST}/request"),
        Some(json!({"title": "Lunch", "who": "sam"})),
        "a1",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        (list("outbox_event").await.len(), list("note").await.len()),
        (1, 1)
    );

    // A failed create announces nothing, and an entity without the property stays silent.
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/{INST}/request"),
        Some(json!({"who": "sam"})),
        "a2",
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/{INST}/plain"),
        Some(json!({"title": "x"})),
        "a3",
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(list("outbox_event").await.len(), 1);
    tokio::task::spawn_blocking(move || drop(app))
        .await
        .unwrap();
}
