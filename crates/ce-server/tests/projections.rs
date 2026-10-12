//! Public projections: unauthenticated reads of allowlisted fields and rows, per workspace.
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
const INST: &str = "pj-1";

fn token(tenant: &str, role: &str, sub: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{sub}","tenant":"{tenant}","roles":["{role}"],"exp":2000}}"#
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

/// Drops every handle to the app away from the async runtime: closing the database client
/// blocks, which is not allowed on a runtime thread.
async fn close(apps: Vec<Router>) {
    tokio::task::spawn_blocking(move || drop(apps))
        .await
        .unwrap();
}

struct Api {
    app: Router,
    n: u32,
    role: &'static str,
    extra: Option<String>,
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
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(T1, self.role, self.role)),
            )
            .header("idempotency-key", key);
        if let Some(c) = &self.extra {
            req = req.header("x-ce-changeset", c.as_str());
        }
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
}

const T2: &str = "22222222-2222-2222-2222-222222222222";

fn manifest(public: bool) -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"slot": {"fields": {
                "starts_at": {"type": "string", "required": true, "indexed": true},
                "status": {"type": "enum", "values": ["held", "confirmed"], "indexed": true},
                "email": {"type": "string", "pii": true}
            }, "sort_keys": ["starts_at"]}},
            "projections": {"open_slots": {"entity": "slot", "fields": ["starts_at"],
                "filter": "status == 'confirmed' and starts_at >= '09'"}},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}},
            "runtime": {"network": {"public_projections": public}}
        }),
        &[],
    )
    .unwrap()
}

async fn setup(public: bool) -> Option<(Api, Router)> {
    let url = url()?;
    let app = tokio::task::spawn_blocking(move || {
        let m = validate(manifest(public).to_string().as_bytes()).unwrap();
        let schema = format!(
            "ce_pj_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST)
            .with_public_tenants(std::sync::Arc::new(|w: &str| match w {
                "acme" => Some(T1.to_string()),
                "other" => Some(T2.to_string()),
                _ => None,
            }));
        st.now = std::sync::Arc::new(|| 1000);
        router(st)
    })
    .await
    .unwrap();
    let api = Api {
        app: app.clone(),
        n: 0,
        role: "owner",
        extra: None,
    };
    Some((api, app))
}

async fn public(app: &Router, uri: &str) -> (StatusCode, Value, axum::http::HeaderMap) {
    let res = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (s, h) = (res.status(), res.headers().clone());
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null), h)
}

#[tokio::test]
async fn a_public_projection_shows_only_allowed_rows_and_fields() {
    let Some((mut owner, app)) = setup(true).await else {
        return;
    };
    for (at, status, email) in [
        ("10:00", "confirmed", "a@x"),
        ("11:00", "held", "b@x"),
        ("08:00", "confirmed", "c@x"),
        ("12:00", "confirmed", "d@x"),
    ] {
        let (s, v) = owner
            .post(
                "slot",
                json!({"starts_at": at, "status": status, "email": email}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let (s, v, h) = public(&app, &format!("/public/{INST}/acme/open_slots")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rows = v["data"].as_array().unwrap();
    let mut times: Vec<&str> = rows
        .iter()
        .map(|r| r["starts_at"].as_str().unwrap())
        .collect();
    times.sort();
    assert_eq!(times, ["10:00", "12:00"]);
    for r in rows {
        assert_eq!(
            r.as_object().unwrap().len(),
            1,
            "only the projected field: {r}"
        );
    }
    assert!(h["cache-control"].to_str().unwrap().contains("public"));
    // Paging: one at a time, caller cannot widen anything.
    let (_, p1, _) = public(
        &app,
        &format!("/public/{INST}/acme/open_slots?limit=1&filter=%7B%7D&select=email"),
    )
    .await;
    assert_eq!(p1["data"].as_array().unwrap().len(), 1);
    assert!(p1["data"][0].get("email").is_none());
    let next = p1["meta"]["next"].as_str().unwrap().to_string();
    let (_, p2, _) = public(
        &app,
        &format!("/public/{INST}/acme/open_slots?limit=1&after={next}"),
    )
    .await;
    assert_eq!(p2["data"].as_array().unwrap().len(), 1);
    assert_ne!(p1["data"][0], p2["data"][0]);
    // Another workspace sees only its own (empty) data; unknown ones and unknown projections 404.
    let (s, v, _) = public(&app, &format!("/public/{INST}/other/open_slots")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["data"].as_array().unwrap().is_empty());
    let (s, _, _) = public(&app, &format!("/public/{INST}/nobody/open_slots")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = public(&app, &format!("/public/{INST}/acme/missing")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    drop(app);
    close(vec![owner.app]).await;
}

#[tokio::test]
async fn public_projections_are_off_unless_the_manifest_turns_them_on() {
    let Some((owner, app)) = setup(false).await else {
        return;
    };
    let (s, _, _) = public(&app, &format!("/public/{INST}/acme/open_slots")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    drop(app);
    close(vec![owner.app]).await;
}
