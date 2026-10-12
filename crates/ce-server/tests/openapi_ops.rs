//! The OpenAPI document covers the routes that are not plain CRUD.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate_composed;
use ce_server::{router, AppState};
use ce_store::MemoryStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";

fn token() -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{T1}","roles":["owner"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

async fn doc(uses: &[&str]) -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let m = ce_tools::compose(&shell, uses).unwrap();
    let m = validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, "oa-1");
    st.now = std::sync::Arc::new(|| 1000);
    let res = router(st)
        .oneshot(
            Request::builder()
                .uri("/_meta/openapi")
                .header(header::AUTHORIZATION, format!("Bearer {}", token()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn batch_is_documented() {
    let d = doc(&["customers"]).await;
    let paths = d["paths"].as_object().unwrap();
    let batch = &paths["/v1/oa-1/_batch"]["post"];
    assert_eq!(batch["operationId"], "batch");
    assert_eq!(
        batch["requestBody"]["content"]["application/json"]["schema"]["properties"]["ops"]
            ["maxItems"],
        100
    );
    assert!(batch["responses"]["409"].is_object());
    assert!(
        paths["/v1/oa-1/customer/_export"]["get"]["responses"]["200"]["content"]["text/csv"]
            .is_object()
    );
    let import = &paths["/v1/oa-1/customer/_import"]["post"];
    assert!(import["responses"]["201"].is_object() && import["responses"]["422"].is_object());
    // Plain tools add no period, changeset or event routes.
    assert!(!paths.keys().any(|k| k.contains("/period/")));
    assert!(!paths.keys().any(|k| k.contains("/changeset/")));
    assert!(!paths.keys().any(|k| k.contains("/events/")));
}

#[tokio::test]
async fn period_changeset_and_event_routes_appear_with_their_tools() {
    let d = doc(&["period", "changesets", "events", "audit"]).await;
    let paths = d["paths"].as_object().unwrap();
    for verb in ["close", "reopen"] {
        let p = &paths[&format!("/v1/oa-1/period/{{id}}/{verb}")]["post"];
        assert_eq!(p["operationId"], format!("period_{verb}"));
        assert!(p["responses"]["409"].is_object());
    }
    for verb in ["merge", "reject", "revert", "withdraw", "preview"] {
        assert!(paths[&format!("/v1/oa-1/changeset/{{id}}/{verb}")]["post"].is_object());
    }
    assert!(paths["/v1/oa-1/events/relay"]["post"]["responses"]["503"].is_object());
    assert!(paths["/v1/oa-1/events/{id}/replay"]["post"].is_object());
    // Every manifest action has a route with its entity's record in the answer.
    let close = &paths["/v1/oa-1/actions/close_period"]["post"];
    assert_eq!(close["operationId"], "action_close_period");
    assert_eq!(
        close["responses"]["200"]["content"]["application/json"]["schema"]["properties"]["data"]
            ["$ref"],
        "#/components/schemas/period"
    );
    let headers = close["parameters"].as_array().unwrap();
    let if_match = headers.iter().find(|h| h["name"] == "If-Match").unwrap();
    assert_eq!(if_match["required"], true);
    assert!(close["responses"]["428"].is_object());
    assert_eq!(
        close["responses"]["200"]["content"]["application/json"]["schema"]["properties"]["meta"]
            ["properties"]["emitted"]["items"]["type"],
        "string"
    );
    assert_eq!(
        close["requestBody"]["content"]["application/json"]["schema"]["required"],
        json!(["id"])
    );
}
