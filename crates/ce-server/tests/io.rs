//! CSV import and export over HTTP.
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
const INST: &str = "io-1";

fn token(tenant: &str, role: &str, sub: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{sub}","tenant":"{tenant}","roles":["{role}"],"exp":9000000}}"#
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

fn build(url: &str, m: Value) -> Router {
    let m = validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_io_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

/// Dropping the last handle closes the database client, which blocks: do it off the runtime.
struct Api {
    app: Option<Router>,
    role: &'static str,
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
    async fn send(
        &mut self,
        method: &str,
        uri: &str,
        ctype: Option<&str>,
        body: &str,
    ) -> (StatusCode, String, Value) {
        self.n += 1;
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(T1, self.role, self.role)),
            )
            .header("idempotency-key", format!("k{}-{}", self.role, self.n));
        if let Some(c) = ctype {
            req = req.header(header::CONTENT_TYPE, c);
        }
        let res = self
            .app
            .clone()
            .unwrap()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, text, json)
    }
    async fn import(&mut self, csv: &str, query: &str) -> (StatusCode, Value) {
        let (s, _, v) = self
            .send(
                "POST",
                &format!("/v1/{INST}/item/_import{query}"),
                Some("text/csv"),
                csv,
            )
            .await;
        (s, v)
    }
    async fn export(&mut self, query: &str) -> (StatusCode, String) {
        let (s, t, _) = self
            .send("GET", &format!("/v1/{INST}/item/_export{query}"), None, "")
            .await;
        (s, t)
    }
    async fn count(&mut self) -> usize {
        let (_, _, v) = self
            .send("GET", &format!("/v1/{INST}/item?limit=100"), None, "")
            .await;
        v["data"].as_array().unwrap().len()
    }
}

fn manifest() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
        "entities": {"item": {
            "fields": {
                "name": {"type": "string", "required": true, "indexed": true},
                "qty": {"type": "integer", "indexed": true},
                "sku": {"type": "string", "unique": true}
            },
            "sort_keys": ["name", "qty"]
        }},
        "roles": {
            "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
            "viewer": {"read": ["*"], "write": [], "actions": []}
        }
    })
}

async fn setup(role: &'static str) -> Option<Api> {
    let url = url()?;
    let app = tokio::task::spawn_blocking(move || build(&url, manifest()))
        .await
        .unwrap();
    Some(Api {
        app: Some(app),
        role,
        n: 0,
    })
}

fn as_role(a: &Api, role: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        role,
        n: 1000,
    }
}

#[tokio::test]
async fn rows_import_in_one_go_and_export_back_with_formula_guard() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    let csv = "name,qty,sku\r\nsoup,3,S1\r\n=cmd,4,S2\r\n\"stew, big\",5,S3\r\n";
    let (s, v) = a.import(csv, "").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["data"]["created"], 3);
    assert_eq!(a.count().await, 3);

    let (s, out) = a.export("?sort=name").await;
    assert_eq!(s, StatusCode::OK);
    let lines: Vec<&str> = out.trim_end().split("\r\n").collect();
    assert_eq!(lines[0], "id,name,qty,sku");
    assert_eq!(lines.len(), 4);
    assert!(out.contains("\"stew, big\""), "{out}");
    assert!(out.contains("'=cmd"), "a formula is guarded: {out}");
}

#[tokio::test]
async fn one_bad_row_imports_nothing_and_a_dry_run_writes_nothing() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    let (s, v) = a.import("name,qty\r\nsoup,3\r\n,4\r\nstew,x\r\n", "").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[2]["ok"], false);
    assert_eq!(a.count().await, 0);

    let (s, v) = a.import("name,qty\r\nsoup,3\r\n", "?dry_run=true").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["created"], 0);
    assert_eq!(a.count().await, 0);
}

#[tokio::test]
async fn a_row_the_database_refuses_rolls_the_whole_import_back() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    let (s, _) = a
        .import("name,sku\r\nfirst,DUP\r\nsecond,DUP\r\n", "")
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(a.count().await, 0);
}

#[tokio::test]
async fn columns_must_be_declared_and_the_caller_must_be_allowed() {
    let Some(mut a) = setup("owner").await else {
        return;
    };
    let (s, v) = a.import("name,bogus\r\nsoup,1\r\n", "").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (s, _) = a.import("name,id\r\nsoup,1\r\n", "").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    let mut viewer = as_role(&a, "viewer");
    let (s, _) = viewer.import("name\r\nsoup\r\n", "").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = viewer.export("").await;
    assert_eq!(s, StatusCode::OK, "reading allows export");
    let (s, _, _) = viewer
        .send("GET", &format!("/v1/{INST}/nope/_export"), None, "")
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
