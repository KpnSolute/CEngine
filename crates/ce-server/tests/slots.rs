//! `GET /v1/{inst}/_slots`: free booking slots from opening hours, bookings and recurrence.
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
const INST: &str = "sl-1";
static N: AtomicU32 = AtomicU32::new(0);

fn token(role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{role}","tenant":"{T1}","roles":["{role}"],"exp":9000000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

struct Api {
    app: Option<Router>,
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
    async fn call(
        &mut self,
        role: &str,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.n += 1;
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", token(role)))
            .header("idempotency-key", format!("k{}", self.n));
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

    async fn make(&mut self, entity: &str, body: Value) -> String {
        let (s, v) = self
            .call(
                "owner",
                "POST",
                &format!("/v1/{INST}/{entity}"),
                None,
                Some(body),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["data"]["id"].as_str().unwrap().to_string()
    }

    async fn slots(&mut self, role: &str, query: &str) -> (StatusCode, Value) {
        self.call(
            role,
            "GET",
            &format!("/v1/{INST}/_slots?{query}"),
            None,
            None,
        )
        .await
    }
}

async fn setup() -> Option<Api> {
    let url = std::env::var("CE_TEST_DATABASE_URL").ok()?;
    let m = ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "salon", "version": "0.1.0", "title": "Salon"},
            "entities": {},
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "guest": {"read": ["resource"], "write": [], "actions": []}
            }
        }),
        &["bookings"],
    )
    .unwrap();
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_sl_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let app = tokio::task::spawn_blocking(move || {
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
        st.now = std::sync::Arc::new(|| 1000);
        router(st)
    })
    .await
    .unwrap();
    Some(Api {
        app: Some(app),
        n: 0,
    })
}

fn weekdays() -> Value {
    json!({"mon": [["09:00", "12:00"]], "tue": [["09:00", "12:00"]]})
}

fn starts(v: &Value) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["starts_at"].as_str().unwrap().to_string())
        .collect()
}

// Monday 2026-10-12 is in US daylight time: 09:00 local is 13:00Z.
const Q: &str = "from=2026-10-12T13:00:00Z&to=2026-10-12T16:00:00Z&duration=60";

#[tokio::test]
async fn slots_avoid_open_bookings_and_ignore_cancelled_ones() {
    let Some(mut a) = setup().await else { return };
    let r = a
        .make(
            "resource",
            json!({"name": "Chair 1", "timezone": "America/New_York", "hours": weekdays()}),
        )
        .await;
    let other = a
        .make(
            "resource",
            json!({"name": "Chair 2", "timezone": "America/New_York", "hours": weekdays()}),
        )
        .await;
    let q = format!("resource={r}&{Q}");
    let (s, v) = a.slots("owner", &q).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(starts(&v).len(), 3, "{v}");

    // 09:00-10:00 local confirmed; 10:00-11:00 cancelled; another resource's booking is no clash.
    let b1 = a
        .make("booking", json!({"resource": r, "contact_name": "Ann", "starts_at": "2026-10-12T13:00:00Z", "ends_at": "2026-10-12T14:00:00Z"}))
        .await;
    let b2 = a
        .make("booking", json!({"resource": r, "contact_name": "Bo", "starts_at": "2026-10-12T14:00:00Z", "ends_at": "2026-10-12T15:00:00Z"}))
        .await;
    a.make("booking", json!({"resource": other, "contact_name": "Cy", "starts_at": "2026-10-12T15:00:00Z", "ends_at": "2026-10-12T16:00:00Z"}))
        .await;
    let (s, v) = a
        .call(
            "owner",
            "POST",
            &format!("/v1/{INST}/actions/cancel_booking"),
            Some(1),
            Some(json!({"id": b2})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let _ = b1;
    let (s, v) = a.slots("owner", &q).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        starts(&v),
        ["2026-10-12T14:00:00+00:00", "2026-10-12T15:00:00+00:00"]
    );
}

#[tokio::test]
async fn a_recurring_booking_blocks_its_later_weeks_and_the_buffer_widens_it() {
    let Some(mut a) = setup().await else { return };
    let r = a
        .make(
            "resource",
            json!({"name": "Room", "timezone": "America/New_York", "hours": weekdays(), "buffer_minutes": 30}),
        )
        .await;
    // Weekly from Monday 2026-10-05 at 10:00 local (14:00Z), one hour.
    a.make("booking", json!({"resource": r, "contact_name": "Dee", "starts_at": "2026-10-05T14:00:00Z", "ends_at": "2026-10-05T15:00:00Z", "recurrence": "FREQ=WEEKLY;COUNT=4"}))
        .await;
    let (s, v) = a.slots("owner", &format!("resource={r}&{Q}")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // 10:00-11:00 is taken and the 30 minute buffer rules out 09:00 and 11:00 as well.
    assert!(starts(&v).is_empty(), "{v}");
    // Half-hour slots (step 30) leave nothing within 30 minutes of the booking either.
    let (_, v) = a
        .slots("owner", &format!("resource={r}&{Q}&duration=30&step=30"))
        .await;
    assert_eq!(
        starts(&v),
        ["2026-10-12T13:00:00+00:00", "2026-10-12T15:30:00+00:00"],
        "{v}"
    );
}

#[tokio::test]
async fn bad_requests_are_refused() {
    let Some(mut a) = setup().await else { return };
    let r = a
        .make(
            "resource",
            json!({"name": "A", "timezone": "America/New_York", "hours": weekdays()}),
        )
        .await;
    let huge = a
        .make(
            "resource",
            json!({"name": "C", "timezone": "America/New_York", "hours": weekdays(), "buffer_minutes": 9223372036854775807i64}),
        )
        .await;
    let nohours = a
        .make(
            "resource",
            json!({"name": "B", "timezone": "America/New_York"}),
        )
        .await;
    for (q, path) in [
        (
            format!("resource={r}&to=2026-10-12T16:00:00Z&duration=60"),
            "/from",
        ),
        (
            format!("resource={r}&from=yesterday&to=2026-10-12T16:00:00Z&duration=60"),
            "/from",
        ),
        (
            format!("resource={r}&from=2026-10-12T16:00:00Z&to=2026-10-12T13:00:00Z&duration=60"),
            "/to",
        ),
        (format!("resource={r}&{Q}&duration=abc"), "/duration"),
        (format!("resource={r}&{Q}&duration=0"), "/duration"),
        (
            format!("resource={r}&{Q}&duration=9223372036854775807"),
            "/duration",
        ),
        (
            format!("resource={r}&{Q}&step=9223372036854775807"),
            "/step",
        ),
        (format!("resource={huge}&{Q}"), "/buffer_minutes"),
        (format!("resource={nohours}&{Q}"), "/hours"),
    ] {
        let (s, v) = a.slots("owner", &q).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{q}: {v}");
        assert_eq!(v["errors"][0]["path"], path, "{q}: {v}");
    }
    let (s, _) = a.slots("owner", &format!("resource=missing&{Q}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A role that cannot read bookings gets nothing.
    let (s, _) = a.slots("guest", &format!("resource={r}&{Q}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_recurring_booking_does_not_break_a_full_year_window() {
    let Some(mut a) = setup().await else { return };
    let r = a
        .make(
            "resource",
            json!({"name": "Year", "timezone": "America/New_York", "hours": weekdays(), "buffer_minutes": 60}),
        )
        .await;
    a.make("booking", json!({"resource": r, "contact_name": "Eve", "starts_at": "2026-10-05T14:00:00Z", "ends_at": "2026-10-05T15:00:00Z", "recurrence": "FREQ=WEEKLY;COUNT=2"}))
        .await;
    // Exactly 366 days.
    let q = format!("resource={r}&from=2026-10-12T13:00:00Z&to=2027-10-13T13:00:00Z&duration=60");
    let (s, v) = a.slots("owner", &q).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // The occurrence on Oct 12 (10:00 local) and its buffer block 09:00-12:00 that Monday.
    assert!(
        !starts(&v).contains(&"2026-10-12T13:00:00+00:00".to_string()),
        "{v}"
    );
}
