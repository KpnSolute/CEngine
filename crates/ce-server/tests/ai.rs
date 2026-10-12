//! `POST /v1/{inst}/_ai/entry`: AI drafts are untrusted, checked like creates and never saved.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::{TenantScope, Verifier};
use ce_manifest::validate;
use ce_server::{router, AppState, Provider, ProviderError, ProviderRequest, ProviderResponse};
use ce_store::MemoryStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "ai-1";

fn token(role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u","tenant":"{T1}","roles":["{role}"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

#[derive(Clone)]
struct Fake {
    answer: Arc<Mutex<Result<Value, ProviderError>>>,
    seen: Arc<Mutex<Vec<(String, ProviderRequest)>>>,
}

impl Provider for Fake {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.seen
            .lock()
            .unwrap()
            .push((scope.tenant_id().to_string(), req.clone()));
        self.answer
            .lock()
            .unwrap()
            .clone()
            .map(|data| ProviderResponse { data })
    }
}

fn build(fake: Option<&Fake>) -> Router {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let m = validate(v.to_string().as_bytes()).unwrap();
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, INST);
    st.now = Arc::new(|| 1000);
    if let Some(f) = fake {
        st = st.with_provider(Arc::new(f.clone()));
    }
    router(st)
}

fn fake(answer: Result<Value, ProviderError>) -> Fake {
    Fake {
        answer: Arc::new(Mutex::new(answer)),
        seen: Arc::default(),
    }
}

async fn entry(app: &Router, role: &str, body: Value) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/{INST}/_ai/entry"))
                .header(header::AUTHORIZATION, format!("Bearer {}", token(role)))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
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
async fn drafts_are_checked_cut_down_and_not_saved() {
    let f = fake(Ok(json!({"records": [
        {"name": "Flour 50lb", "category": "dry", "unit_cost": "21.5000", "on_hand": "4"},
        {"name": "Mystery", "category": "snacks", "unit_cost": "1"},
        {"name": "Rice", "unit_cost": "2", "value": "999", "tenant_id": "x", "notes": null},
        "not an object"
    ]})));
    let app = build(Some(&f));
    let (s, v) = entry(
        &app,
        "manager",
        json!({"entity": "item", "text": "2 bags flour 50lb at 21.50", "hint": "invoice"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // A draft that needed fields dropped is not ready: a person looks at it first.
    assert_eq!(v["data"]["ready"], 1, "{v}");
    let d = v["data"]["drafts"].as_array().unwrap();
    assert_eq!(d.len(), 3, "the non-object is skipped: {v}");
    assert_eq!(d[0]["errors"], json!([]));
    assert_eq!(d[1]["errors"][0]["path"], "/category");
    // A computed field and an engine column are dropped from the draft and reported.
    assert_eq!(d[2]["body"], json!({"name": "Rice", "unit_cost": "2"}));
    let paths: Vec<&str> = d[2]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["/notes", "/tenant_id", "/value"]);

    // The provider was told the entity's shape and the text, and the caller's tenant, nothing else.
    let seen = f.seen.lock().unwrap();
    let (tenant, req) = &seen[0];
    assert_eq!(tenant, T1);
    assert_eq!(
        (req.connector.name(), req.operation.as_str()),
        ("ai", "records.extract")
    );
    assert_eq!(req.params["entity"], "item");
    assert_eq!(req.params["text"], "2 bags flour 50lb at 21.50");
    assert!(
        req.params["fields"].get("value").is_none(),
        "computed fields are not offered"
    );
    assert_eq!(req.params["fields"]["category"]["values"][0], "dry");
}

#[tokio::test]
async fn access_and_input_rules() {
    let f = fake(Ok(json!({"records": []})));
    let app = build(Some(&f));
    let ok = json!({"entity": "item", "text": "x"});
    // Staff may read but not write items, so they may not draft them.
    assert_eq!(
        entry(&app, "staff", ok.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        entry(&app, "manager", json!({"entity": "nope", "text": "x"}))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for bad in [
        json!({"text": "x"}),
        json!({"entity": "item"}),
        json!({"entity": "item", "text": "   "}),
        json!({"entity": "item", "text": "x", "hint": 5}),
        json!({"entity": "item", "text": "x".repeat(20_001)}),
    ] {
        assert_eq!(
            entry(&app, "manager", bad.clone()).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}"
        );
    }
    assert!(
        f.seen.lock().unwrap().is_empty(),
        "no provider call was made for any refused request"
    );
}

#[tokio::test]
async fn provider_trouble_has_safe_answers() {
    let ok = json!({"entity": "item", "text": "x"});
    let (s, _) = entry(&build(None), "manager", ok.clone()).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);

    let f = fake(Err(ProviderError::Unavailable));
    let (s, v) = entry(&build(Some(&f)), "manager", ok.clone()).await;
    assert_eq!(
        (s, v["retryable"].clone()),
        (StatusCode::SERVICE_UNAVAILABLE, json!(true))
    );

    let f = fake(Err(ProviderError::Failed(
        "model key sk-secret rejected".into(),
    )));
    let (s, v) = entry(&build(Some(&f)), "manager", ok.clone()).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert!(!v.to_string().contains("sk-secret"), "{v}");

    let f = fake(Ok(json!({"answer": "hello"})));
    let (s, _) = entry(&build(Some(&f)), "manager", ok).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn escaped_text_within_the_limit_is_accepted_and_server_fields_are_not_ready() {
    // 20,000 control characters are 120,000 bytes once JSON-escaped: still within the text limit.
    let f = fake(Ok(json!({"records": []})));
    let app = build(Some(&f));
    let text = "\u{1}".repeat(20_000);
    let (s, v) = entry(&app, "manager", json!({"entity": "item", "text": text})).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // With the period tool, fields the server alone writes keep a draft from being ready.
    let m = ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        }),
        &["period"],
    )
    .unwrap();
    let m = validate(m.to_string().as_bytes()).unwrap();
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, INST);
    st.now = Arc::new(|| 1000);
    let f = fake(Ok(json!({"records": [
        {"label": "2026-09", "starts_on": "2026-09-01", "ends_on": "2026-09-30"},
        {"label": "2026-10", "starts_on": "2026-10-01", "ends_on": "2026-10-31", "status": "closed"}
    ]})));
    let app = router(st.with_provider(Arc::new(f)));
    let (s, v) = entry(&app, "owner", json!({"entity": "period", "text": "months"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["ready"], 1, "{v}");
    assert_eq!(
        v["data"]["drafts"][1]["errors"][0]["code"],
        "server_managed"
    );
}
