//! Files: bytes go to a blob store, the record keeps the metadata; reads need read access.
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
const INST: &str = "fl-1";

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
    async fn list(&mut self, entity: &str) -> Vec<Value> {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/{entity}?limit=100"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].as_array().unwrap().clone()
    }
}

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"doc": {
                "fields": {
                    "title": {"type": "string", "required": true},
                    "scan": {"type": "file"},
                    "frozen": {"type": "boolean", "default": false}
                },
                "locked_when": "frozen == true"
            }},
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "dropbox": {"read": [], "write": ["doc"], "actions": []}
            },
            "runtime": {"storage": {"relational": true, "files": true, "max_file_mb": 1}}
        }),
        &["audit"],
    )
    .unwrap()
}

struct Files {
    api: Api,
    blobs: std::sync::Arc<ce_server::MemoryBlobs>,
}

async fn setup() -> Option<Files> {
    let url = url()?;
    let blobs = std::sync::Arc::new(ce_server::MemoryBlobs::new());
    let b2 = blobs.clone();
    let app = tokio::task::spawn_blocking(move || {
        let m = validate(manifest().to_string().as_bytes()).unwrap();
        let schema = format!(
            "ce_fl_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST).with_blobs(b2);
        st.now = std::sync::Arc::new(|| 1000);
        router(st)
    })
    .await
    .unwrap();
    Some(Files {
        api: Api {
            app,
            n: 0,
            role: "owner",
            extra: None,
        },
        blobs,
    })
}

async fn raw(
    app: &Router,
    method: &str,
    uri: &str,
    role: &str,
    version: Option<u64>,
    body: Vec<u8>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", token(T1, role, role)),
        )
        .header(header::CONTENT_TYPE, "application/pdf")
        .header("x-ce-filename", "../scan 1.pdf");
    if let Some(v) = version {
        req = req.header("if-match", format!("\"{v}\""));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (res.status(), res.headers().clone());
    (
        status,
        headers,
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

fn path(id: &str, field: &str) -> String {
    format!("/v1/{INST}/_files/doc/{id}/{field}")
}

#[tokio::test]
async fn a_file_is_stored_hashed_and_read_back() {
    let Some(mut f) = setup().await else { return };
    let (_, d) = f.api.post("doc", json!({"title": "lease"})).await;
    let id = d["data"]["id"].as_str().unwrap().to_string();
    let bytes = b"%PDF-1.4 hello".to_vec();
    let (s, _, body) = raw(
        &f.api.app,
        "PUT",
        &path(&id, "scan"),
        "owner",
        Some(1),
        bytes.clone(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: Value = serde_json::from_slice(&body).unwrap();
    let meta = &v["data"]["scan"];
    assert_eq!(meta["size"], bytes.len());
    assert_eq!(meta["content_type"], "application/pdf");
    assert_eq!(meta["name"], "scan 1.pdf", "name is cleaned");
    assert_eq!(meta["sha256"].as_str().unwrap().len(), 64);
    assert!(meta["key"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{T1}/doc/{id}/scan/")));
    assert_eq!(v["data"]["version"], 2);
    assert_eq!(f.blobs.len(), 1);

    let (s, h, got) = raw(&f.api.app, "GET", &path(&id, "scan"), "owner", None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got, bytes);
    assert_eq!(h["content-type"], "application/pdf");
    assert_eq!(
        h["etag"].to_str().unwrap().trim_matches('"'),
        meta["sha256"].as_str().unwrap()
    );
    assert!(h["content-disposition"]
        .to_str()
        .unwrap()
        .contains("scan 1.pdf"));
    // create + title patch (the upload) = 2 audit rows.
    assert_eq!(f.api.list("audit_event").await.len(), 2);
    close(vec![f.api.app]).await;
}

#[tokio::test]
async fn files_obey_roles_sizes_fields_freezes_and_forged_keys() {
    let Some(mut f) = setup().await else { return };
    let (_, d) = f.api.post("doc", json!({"title": "a"})).await;
    let id = d["data"]["id"].as_str().unwrap().to_string();
    let (_, e) = f.api.post("doc", json!({"title": "b"})).await;
    let other = e["data"]["id"].as_str().unwrap().to_string();
    let app = f.api.app.clone();
    // Write-only role may upload (and gets no record back) but not download.
    let (s, _, body) = raw(
        &app,
        "PUT",
        &path(&id, "scan"),
        "dropbox",
        Some(1),
        b"x".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert!(v["data"].get("title").is_none());
    let (s, _, _) = raw(&app, "GET", &path(&id, "scan"), "dropbox", None, vec![]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Version needed; empty body; wrong field; too large.
    let (s, _, _) = raw(
        &app,
        "PUT",
        &path(&id, "scan"),
        "owner",
        None,
        b"x".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::PRECONDITION_REQUIRED);
    let (s, _, _) = raw(&app, "PUT", &path(&id, "scan"), "owner", Some(2), vec![]).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _, _) = raw(
        &app,
        "PUT",
        &path(&id, "title"),
        "owner",
        Some(2),
        b"x".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = raw(
        &app,
        "PUT",
        &path(&id, "scan"),
        "owner",
        Some(2),
        vec![0u8; 1024 * 1024 + 1],
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    // The advertised hash comes from the key, not from a stored value a patch could set.
    let (_, mine) = f
        .api
        .call("GET", &format!("/v1/{INST}/doc/{id}"), None, None)
        .await;
    let mut meta = mine["data"]["scan"].clone();
    let real = meta["key"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    meta["sha256"] = json!("bogus");
    meta["content_type"] = json!("text/plain");
    let (s, v) = f
        .api
        .call(
            "PATCH",
            &format!("/v1/{INST}/doc/{id}"),
            Some(2),
            Some(json!({"scan": meta})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, h, _) = raw(&app, "GET", &path(&id, "scan"), "owner", None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["etag"].to_str().unwrap().trim_matches('"'), real);
    let (s, _, spec) = raw(&app, "GET", "/_meta/openapi", "owner", None, vec![]).await;
    assert_eq!(s, StatusCode::OK);
    let spec: Value = serde_json::from_slice(&spec).unwrap();
    let p = &spec["paths"][format!("/v1/{INST}/_files/doc/{{id}}/scan")];
    assert!(p["get"].is_object() && p["put"].is_object(), "{p}");
    // A plain patch cannot point a file field at another record's file.
    let (s, _, _) = raw(
        &app,
        "PUT",
        &path(&other, "scan"),
        "owner",
        Some(1),
        b"secret".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, got) = f
        .api
        .call("GET", &format!("/v1/{INST}/doc/{other}"), None, None)
        .await;
    let forged = got["data"]["scan"].clone();
    let (s, v) = f
        .api
        .call(
            "PATCH",
            &format!("/v1/{INST}/doc/{id}"),
            Some(3),
            Some(json!({"scan": forged})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _, _) = raw(&app, "GET", &path(&id, "scan"), "owner", None, vec![]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "forged key must not resolve");
    // A frozen record takes no upload.
    let (s, v) = f
        .api
        .call(
            "PATCH",
            &format!("/v1/{INST}/doc/{other}"),
            Some(2),
            Some(json!({"frozen": true})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _, _) = raw(
        &app,
        "PUT",
        &path(&other, "scan"),
        "owner",
        Some(3),
        b"more".to_vec(),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    drop(app);
    close(vec![f.api.app]).await;
}
