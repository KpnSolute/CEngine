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
use serde_json::{json, Value};
use sha2::Sha256;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
const INST: &str = "inv-1";

fn token(tenant: &str, roles: &[&str], exp: u64) -> String {
    let list = roles
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(",");
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{tenant}","roles":[{list}],"exp":{exp}}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn manager(tenant: &str) -> String {
    token(tenant, &["manager"], 2000)
}

fn app() -> Router {
    app_with(|_| {})
}

fn app_with(edit: impl FnOnce(&mut Value)) -> Router {
    let path = format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    edit(&mut v);
    let m = validate(v.to_string().as_bytes()).expect("edited manifest is still valid");
    let mut st = AppState::new(MemoryStore::new(), Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    tok: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Value>,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = tok {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    let res = app
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
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Resp {
        status,
        headers,
        body,
    }
}

fn item() -> Value {
    json!({"name": "flour", "unit_cost": 2.5})
}

async fn create(app: &Router, tok: &str, key: &str, body: Value) -> Resp {
    call(
        app,
        "POST",
        &format!("/v1/{INST}/item"),
        Some(tok),
        &[("idempotency-key", key)],
        Some(body),
    )
    .await
}

#[tokio::test]
async fn health_needs_no_token() {
    let a = app();
    let r = call(&a, "GET", "/_meta/health/live", None, &[], None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["status"], "ok");
}

#[tokio::test]
async fn missing_bad_and_expired_tokens_are_401_problems() {
    let a = app();
    let uri = format!("/v1/{INST}/item/x");
    let expired = token(T1, &["manager"], 500);
    for tok in [None, Some("garbage"), Some(expired.as_str())] {
        let r = call(&a, "GET", &uri, tok, &[], None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert_eq!(r.headers[header::CONTENT_TYPE], "application/problem+json");
        assert_eq!(r.headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(r.body["code"], "unauthenticated");
        assert!(!r.body["request_id"].as_str().unwrap().is_empty());
    }
}

#[tokio::test]
async fn roles_decide_read_and_write() {
    let a = app();
    let staff = token(T1, &["staff"], 2000);
    // staff cannot write
    let r = create(&a, &staff, "k1", item()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["code"], "forbidden");
    // manager creates, staff can read
    let made = create(&a, &manager(T1), "k2", item()).await;
    let id = made.body["data"]["id"].as_str().unwrap().to_string();
    let r = call(
        &a,
        "GET",
        &format!("/v1/{INST}/item/{id}"),
        Some(&staff),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    // staff cannot read period; a role-less token cannot read anything
    let r = call(
        &a,
        "GET",
        &format!("/v1/{INST}/period/x"),
        Some(&staff),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let nobody = token(T1, &[], 2000);
    let r = call(
        &a,
        "GET",
        &format!("/v1/{INST}/item/{id}"),
        Some(&nobody),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unknown_entities_and_installations_do_not_leak() {
    let a = app();
    let staff = token(T1, &["staff"], 2000);
    // without a grant, unknown and known-but-denied entities look the same
    let known = call(
        &a,
        "GET",
        &format!("/v1/{INST}/period/x"),
        Some(&staff),
        &[],
        None,
    )
    .await;
    let unknown = call(
        &a,
        "GET",
        &format!("/v1/{INST}/nothing/x"),
        Some(&staff),
        &[],
        None,
    )
    .await;
    assert_eq!(known.status, unknown.status);
    assert_eq!(known.body["code"], unknown.body["code"]);
    // a wildcard reader gets 404 for an entity that does not exist
    let r = call(
        &a,
        "GET",
        &format!("/v1/{INST}/nothing/x"),
        Some(&manager(T1)),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = call(&a, "GET", "/v1/other/item/x", Some(&manager(T1)), &[], None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_then_read_with_etag_and_location() {
    let a = app();
    let r = create(&a, &manager(T1), "k1", item()).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.body["data"]["id"].as_str().unwrap().to_string();
    assert_eq!(r.body["data"]["version"], 1);
    assert_eq!(r.body["data"]["name"], "flour");
    assert_eq!(r.headers[header::ETAG], "\"1\"");
    assert_eq!(
        r.headers[header::LOCATION],
        format!("/v1/{INST}/item/{id}").as_str()
    );
    let uri = format!("/v1/{INST}/item/{id}");
    let g = call(&a, "GET", &uri, Some(&manager(T1)), &[], None).await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body["data"]["unit_cost"], 2.5);
    let nm = call(
        &a,
        "GET",
        &uri,
        Some(&manager(T1)),
        &[("if-none-match", "\"1\"")],
        None,
    )
    .await;
    assert_eq!(nm.status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn idempotency_key_is_required_and_replays() {
    let a = app();
    let tok = manager(T1);
    let r = call(
        &a,
        "POST",
        &format!("/v1/{INST}/item"),
        Some(&tok),
        &[],
        Some(item()),
    )
    .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(r.body["code"], "precondition_required");
    let first = create(&a, &tok, "same", item()).await;
    let again = create(&a, &tok, "same", item()).await;
    assert_eq!(first.body["data"]["id"], again.body["data"]["id"]);
    let other = create(&a, &tok, "same", json!({"name": "sugar", "unit_cost": 1})).await;
    assert_eq!(other.status, StatusCode::CONFLICT);
    assert_eq!(other.body["code"], "idempotency_conflict");
}

#[tokio::test]
async fn bodies_are_validated() {
    let a = app();
    let tok = manager(T1);
    let r = create(&a, &tok, "k1", json!([1, 2])).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = create(&a, &tok, "k2", json!({"name": "x"})).await;
    assert_eq!(r.body["errors"][0]["path"], "/unit_cost");
    assert_eq!(r.body["errors"][0]["code"], "required");
    let r = create(
        &a,
        &tok,
        "k3",
        json!({"name": "x", "unit_cost": 1, "id": "mine", "tenant_id": T2, "nope": 1, "value": 3}),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let codes: Vec<(String, String)> = r.body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().into(),
                e["code"].as_str().unwrap().into(),
            )
        })
        .collect();
    for want in [
        ("/id", "reserved_field"),
        ("/tenant_id", "reserved_field"),
        ("/nope", "unknown_field"),
        ("/value", "computed_field"),
    ] {
        assert!(
            codes.contains(&(want.0.into(), want.1.into())),
            "{want:?} in {codes:?}"
        );
    }
    // not JSON at all
    let res = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/{INST}/item"))
                .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                .header("idempotency-key", "k4")
                .body(Body::from("{oops"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn patch_needs_if_match_and_merges() {
    let a = app();
    let tok = manager(T1);
    let made = create(
        &a,
        &tok,
        "k1",
        json!({"name": "flour", "unit_cost": 2, "category": "dry"}),
    )
    .await;
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[],
        Some(json!({"name": "bread flour"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_REQUIRED);
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"7\"")],
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["code"], "version_conflict");
    assert_eq!(r.body["current_version"], 1);
    assert_eq!(r.body["retryable"], true);
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "banana")],
        Some(json!({"name": "x"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // set one field, remove another with null, leave the rest
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"name": "bread flour", "category": null})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["data"]["version"], 2);
    assert_eq!(r.body["data"]["name"], "bread flour");
    assert_eq!(r.body["data"]["unit_cost"], 2);
    assert!(r.body["data"].get("category").is_none());
    assert_eq!(r.headers[header::ETAG], "\"2\"");
    // the old version no longer works
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"name": "y"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.body["current_version"], 2);
}

#[tokio::test]
async fn delete_is_soft_and_versioned() {
    let a = app();
    let tok = manager(T1);
    let made = create(&a, &tok, "k1", item()).await;
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(&a, "DELETE", &uri, Some(&tok), &[], None).await;
    assert_eq!(r.status, StatusCode::PRECONDITION_REQUIRED);
    let r = call(
        &a,
        "DELETE",
        &uri,
        Some(&tok),
        &[("if-match", "\"9\"")],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = call(
        &a,
        "DELETE",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(&a, "GET", &uri, Some(&tok), &[], None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tenants_cannot_see_each_other() {
    let a = app();
    let made = create(&a, &manager(T1), "k1", item()).await;
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    for (method, extra) in [
        ("GET", vec![]),
        ("PATCH", vec![("if-match", "\"1\"")]),
        ("DELETE", vec![("if-match", "\"1\"")]),
    ] {
        let body = (method == "PATCH").then(|| json!({"name": "stolen"}));
        let r = call(&a, method, &uri, Some(&manager(T2)), &extra, body).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{method}");
    }
    // the owner still sees it untouched
    let r = call(&a, "GET", &uri, Some(&manager(T1)), &[], None).await;
    assert_eq!(r.body["data"]["name"], "flour");
    assert_eq!(r.body["data"]["version"], 1);
}

#[tokio::test]
async fn oversized_bodies_are_refused() {
    let a = app();
    let big = "x".repeat(2 * 1024 * 1024);
    let r = create(&a, &manager(T1), "k1", json!({"name": big, "unit_cost": 1})).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/problem+json");
    assert_eq!(r.body["code"], "payload_too_large");
    assert!(!r.body["request_id"].as_str().unwrap().is_empty());
    // token check still comes first for an oversized body
    let r = call(
        &a,
        "POST",
        &format!("/v1/{INST}/item"),
        None,
        &[("idempotency-key", "k2")],
        Some(json!({"name": big, "unit_cost": 1})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn write_only_roles_never_get_stored_fields_back() {
    let a = app_with(|m| {
        m["roles"]["dropbox"] = json!({"read": [], "write": ["item"]});
    });
    let drop = token(T1, &["dropbox"], 2000);
    let made = create(&a, &drop, "k1", item()).await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert_eq!(
        made.body["data"],
        json!({"id": made.body["data"]["id"], "version": 1})
    );
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    // an empty patch must not reveal the record either
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&drop),
        &[("if-match", "\"1\"")],
        Some(json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.body["data"],
        json!({"id": made.body["data"]["id"], "version": 2})
    );
    // and it cannot read
    let r = call(&a, "GET", &uri, Some(&drop), &[], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // a reader still gets the full record
    let r = call(&a, "GET", &uri, Some(&manager(T1)), &[], None).await;
    assert_eq!(r.body["data"]["name"], "flour");
}

#[tokio::test]
async fn patch_cannot_remove_required_fields() {
    let a = app();
    let tok = manager(T1);
    let made = create(&a, &tok, "k1", item()).await;
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"name": null})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["errors"][0]["path"], "/name");
    assert_eq!(r.body["errors"][0]["code"], "required");
    let r = call(&a, "GET", &uri, Some(&tok), &[], None).await;
    assert_eq!(r.body["data"]["name"], "flour");
    assert_eq!(r.body["data"]["version"], 1);
}

#[tokio::test]
async fn required_computed_fields_are_not_asked_of_callers() {
    let a = app_with(|m| {
        m["entities"]["item"]["fields"]["value"]["required"] = json!(true);
    });
    let r = create(&a, &manager(T1), "k1", item()).await;
    assert_eq!(r.status, StatusCode::CREATED);
}

#[tokio::test]
async fn manifest_and_openapi_are_served_to_authenticated_callers() {
    let a = app();
    let r = call(&a, "GET", "/_meta/openapi", None, &[], None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = call(&a, "GET", "/_meta/manifest", Some(&manager(T1)), &[], None).await;
    assert_eq!(r.body["template"]["name"], "inventory");
    let r = call(&a, "GET", "/_meta/openapi", Some(&manager(T1)), &[], None).await;
    assert_eq!(r.status, StatusCode::OK);
    let doc = r.body;
    assert_eq!(doc["openapi"], "3.1.0");
    for p in [
        format!("/v1/{INST}/item"),
        format!("/v1/{INST}/item/{{id}}"),
        format!("/v1/{INST}/period"),
    ] {
        assert!(doc["paths"][p.as_str()].is_object(), "{p}");
    }
    let post = &doc["paths"][format!("/v1/{INST}/item")]["post"];
    assert_eq!(post["parameters"][0]["name"], "Idempotency-Key");
    // The create body takes an optional writable `id`; the record schema keeps it read-only.
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/itemCreate"
    );
    let create = &doc["components"]["schemas"]["itemCreate"];
    assert_eq!(create["properties"]["id"]["format"], "uuid");
    assert!(create["properties"]["id"]["readOnly"].is_null());
    assert!(create["properties"]["version"].is_null());
    assert_eq!(
        doc["components"]["schemas"]["item"]["properties"]["id"]["readOnly"],
        true
    );
    assert!(post["responses"]["409"].is_object() && post["responses"]["428"].is_object());
    let item = &doc["components"]["schemas"]["item"];
    assert_eq!(item["properties"]["name"]["maxLength"], 120);
    assert_eq!(item["properties"]["value"]["readOnly"], true);
    assert_eq!(item["properties"]["category"]["enum"][0], "dry");
    assert!(item["required"]
        .as_array()
        .unwrap()
        .contains(&json!("unit_cost")));
}

#[tokio::test]
async fn field_types_and_limits_come_from_the_manifest() {
    let a = app();
    let tok = manager(T1);
    let cases = [
        (
            json!({"name": "x", "unit_cost": "abc"}),
            "/unit_cost",
            "invalid_format",
        ),
        (
            json!({"name": "x", "unit_cost": 1.00001}),
            "/unit_cost",
            "out_of_range",
        ),
        (
            json!({"name": "x".repeat(121), "unit_cost": 1}),
            "/name",
            "too_long",
        ),
        (
            json!({"name": "x", "unit_cost": 1, "category": "nope"}),
            "/category",
            "invalid_value",
        ),
        (
            json!({"name": "x", "unit_cost": 1, "period": "not-a-uuid"}),
            "/period",
            "invalid_format",
        ),
        (json!({"name": 7, "unit_cost": 1}), "/name", "type_mismatch"),
    ];
    for (i, (body, path, code)) in cases.into_iter().enumerate() {
        let r = create(&a, &tok, &format!("t{i}"), body).await;
        assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(r.body["errors"][0]["path"], path);
        assert_eq!(r.body["errors"][0]["code"], code);
    }
    // a patch is checked the same way, and nothing is written when it fails
    let made = create(&a, &tok, "ok", item()).await;
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"category": "nope"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let g = call(&a, "GET", &uri, Some(&tok), &[], None).await;
    assert_eq!(g.body["data"]["version"], 1);
}

#[tokio::test]
async fn create_sets_the_initial_state_and_refuses_a_caller_choice() {
    let a = app();
    let tok = manager(T1);
    let path = format!("/v1/{INST}/period");
    let h = [("idempotency-key", "p1")];
    let r = call(
        &a,
        "POST",
        &path,
        Some(&tok),
        &h,
        Some(json!({"month": "2026-01-01"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.body["data"]["state"], "open");
    let h = [("idempotency-key", "p2")];
    let body = json!({"month": "2026-02-01", "state": "published"});
    let r = call(&a, "POST", &path, Some(&tok), &h, Some(body)).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn invariants_refuse_bad_creates_and_patches() {
    let a = app();
    let tok = manager(T1);
    let r = create(
        &a,
        &tok,
        "inv1",
        json!({"name": "flour", "unit_cost": 2, "on_hand": "-1"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["errors"][0]["code"], "invariant_failed");
    assert_eq!(r.body["errors"][0]["detail"], "inventory.negative_stock");

    let made = create(&a, &tok, "inv2", json!({"name": "flour", "unit_cost": 2})).await;
    assert_eq!(made.status, StatusCode::CREATED);
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"on_hand": "-5"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.body["errors"][0]["detail"], "inventory.negative_stock");
    // The rejected patch changed nothing: version is still 1.
    let r = call(&a, "GET", &uri, Some(&tok), &[], None).await;
    assert_eq!(r.body["data"]["version"], 1);
}

#[tokio::test]
async fn computed_fields_are_filled_on_create_and_refreshed_on_patch() {
    let a = app();
    let tok = manager(T1);
    let made = create(
        &a,
        &tok,
        "c1",
        json!({"name": "flour", "unit_cost": "2.5", "on_hand": "4"}),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert_eq!(made.body["data"]["value"], "10.00");
    let uri = format!(
        "/v1/{INST}/item/{}",
        made.body["data"]["id"].as_str().unwrap()
    );
    let r = call(
        &a,
        "PATCH",
        &uri,
        Some(&tok),
        &[("if-match", "\"1\"")],
        Some(json!({"on_hand": "10"})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["data"]["value"], "25.00");
    // Callers still cannot set a computed field themselves.
    let r = create(
        &a,
        &tok,
        "c2",
        json!({"name": "x", "unit_cost": "1", "value": "9.00"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn openapi_lists_q_only_for_searchable_entities() {
    let names = |doc: &Value, p: &str| -> Vec<String> {
        doc["paths"][p]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap().to_string())
            .collect()
    };
    let path = format!("/v1/{INST}/item");
    let a = app_with(|v| v["entities"]["item"]["fields"]["name"]["searchable"] = true.into());
    let r = call(&a, "GET", "/_meta/openapi", Some(&manager(T1)), &[], None).await;
    assert!(names(&r.body, &path).contains(&"q".to_string()));
    let a = app_with(|v| {
        for f in v["entities"]["item"]["fields"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            f.as_object_mut().unwrap().remove("searchable");
        }
    });
    let r = call(&a, "GET", "/_meta/openapi", Some(&manager(T1)), &[], None).await;
    assert!(!names(&r.body, &path).contains(&"q".to_string()));
}
