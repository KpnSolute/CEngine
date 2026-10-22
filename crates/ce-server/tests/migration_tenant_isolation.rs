//! Two synthetic tenants in one installation cannot see each other's rows, files or migration
//! provenance, and the same legacy id in both lineages maps to two different targets.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, AppState, MemoryBlobs};
use ce_store_pg::{connect, Tls, TypedPgStore};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
const INST: &str = "iso-1";

static N: AtomicU32 = AtomicU32::new(0);

fn token(tenant: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"owner","tenant":"{tenant}","roles":["owner"],"exp":99999999}}"#
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

fn manifest() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "iso", "version": "0.1.0", "title": "Iso"},
        "entities": {"doc": {"fields": {
            "title": {"type": "string", "required": true},
            "scan": {"type": "file"}
        }}},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}},
        "runtime": {"storage": {"relational": true, "files": true, "max_file_mb": 1}}
    })
}

struct Rig {
    app: Option<Router>,
    url: String,
    schema: String,
}

impl Rig {
    fn app(&self) -> &Router {
        self.app.as_ref().unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Everything blocking runs on a thread of its own: the store closes with a nested
        // runtime, which aborts the process if it happens on a runtime thread.
        let (app, url, schema) = (self.app.take(), self.url.clone(), self.schema.clone());
        let _ = std::thread::spawn(move || {
            drop(app);
            if let Ok(mut c) = connect(&url, &Tls::Off) {
                let _ = c.batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#));
            }
        })
        .join();
    }
}

async fn rig() -> Option<Rig> {
    let url = url()?;
    let schema = format!(
        "ce_iso_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let (u2, s2) = (url.clone(), schema.clone());
    let app = tokio::task::spawn_blocking(move || {
        let m = validate(manifest().to_string().as_bytes()).unwrap();
        let store = TypedPgStore::install(&u2, &m, &s2).unwrap();
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST)
            .with_blobs(Arc::new(MemoryBlobs::new()));
        st.now = Arc::new(|| 1000);
        router(st)
    })
    .await
    .unwrap();
    Some(Rig {
        app: Some(app),
        url,
        schema,
    })
}

#[allow(clippy::too_many_arguments)]
async fn call_ver(
    app: &Router,
    tenant: &str,
    method: &str,
    uri: &str,
    n: u32,
    body: Option<Value>,
    raw: Option<Vec<u8>>,
    version: Option<u64>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", token(tenant)))
        .header("idempotency-key", format!("{tenant}-{n}"));
    if let Some(v) = version {
        req = req.header("if-match", format!("\"{v}\""));
    }
    let payload = if let Some(b) = raw {
        req = req.header(header::CONTENT_TYPE, "application/pdf");
        Body::from(b)
    } else if let Some(b) = body {
        req = req.header(header::CONTENT_TYPE, "application/json");
        Body::from(b.to_string())
    } else {
        Body::empty()
    };
    let res = app
        .clone()
        .oneshot(req.body(payload).unwrap())
        .await
        .unwrap();
    let s = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call(
    app: &Router,
    tenant: &str,
    method: &str,
    uri: &str,
    n: u32,
    body: Option<Value>,
    raw: Option<Vec<u8>>,
) -> (StatusCode, Value) {
    call_ver(app, tenant, method, uri, n, body, raw, None).await
}

#[tokio::test]
async fn one_tenant_cannot_read_change_or_list_the_others_rows() {
    let Some(r) = rig().await else { return };
    let (s, v) = call(
        r.app(),
        A,
        "POST",
        &format!("/v1/{INST}/doc"),
        1,
        Some(json!({"title": "a-secret"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, _) = call(
        r.app(),
        B,
        "POST",
        &format!("/v1/{INST}/doc"),
        1,
        Some(json!({"title": "b-own"})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, _) = call(
        r.app(),
        B,
        "GET",
        &format!("/v1/{INST}/doc/{id}"),
        2,
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call_ver(
        r.app(),
        B,
        "DELETE",
        &format!("/v1/{INST}/doc/{id}"),
        3,
        None,
        None,
        Some(1),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, v) = call(
        r.app(),
        B,
        "GET",
        &format!("/v1/{INST}/doc?limit=100"),
        4,
        None,
        None,
    )
    .await;
    let titles: Vec<_> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["title"].clone())
        .collect();
    assert_eq!(
        titles,
        vec![json!("b-own")],
        "tenant B sees only its own row"
    );
    // The row is still there for A.
    let (s, _) = call(
        r.app(),
        A,
        "GET",
        &format!("/v1/{INST}/doc/{id}"),
        2,
        None,
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn one_tenant_cannot_fetch_the_others_file_even_with_a_forged_pointer() {
    let Some(r) = rig().await else { return };
    let (_, v) = call(
        r.app(),
        A,
        "POST",
        &format!("/v1/{INST}/doc"),
        1,
        Some(json!({"title": "a"})),
        None,
    )
    .await;
    let a_id = v["data"]["id"].as_str().unwrap().to_string();
    let (s, v) = call_ver(
        r.app(),
        A,
        "PUT",
        &format!("/v1/{INST}/_files/doc/{a_id}/scan"),
        2,
        None,
        Some(b"%PDF a-only".to_vec()),
        Some(1),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let forged = v["data"]["scan"].clone();

    let (s, _) = call(
        r.app(),
        B,
        "GET",
        &format!("/v1/{INST}/_files/doc/{a_id}/scan"),
        1,
        None,
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "B cannot fetch A's file by A's id"
    );

    // B writes A's file pointer into its own record; the key belongs to A, so it must not resolve.
    let (_, v) = call(
        r.app(),
        B,
        "POST",
        &format!("/v1/{INST}/doc"),
        2,
        Some(json!({"title": "b"})),
        None,
    )
    .await;
    let b_id = v["data"]["id"].as_str().unwrap().to_string();
    let _ = call(
        r.app(),
        B,
        "PATCH",
        &format!("/v1/{INST}/doc/{b_id}"),
        3,
        Some(json!({"scan": forged})),
        None,
    )
    .await;
    let (s, _) = call(
        r.app(),
        B,
        "GET",
        &format!("/v1/{INST}/_files/doc/{b_id}/scan"),
        4,
        None,
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a forged pointer to A's key does not resolve for B"
    );
}

#[test]
fn migration_provenance_is_per_tenant_and_a_legacy_id_maps_once_per_tenant() {
    let Some(url) = url() else { return };
    let schema = format!(
        "ce_isop_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let m = validate(manifest().to_string().as_bytes()).unwrap();
    let store = TypedPgStore::install(&url, &m, &schema).unwrap();
    let mut c = connect(&url, &Tls::Off).unwrap();
    let run_a = "a0000000-0000-0000-0000-00000000000a";
    let run_b = "b0000000-0000-0000-0000-00000000000b";
    for (tenant, run) in [(A, run_a), (B, run_b)] {
        c.batch_execute(&format!(
            r#"INSERT INTO "{schema}".migration_runs (id, tenant_id, installation_id, source_system, source_project_ref, migration_version, initiated_by)
               VALUES ('{run}', '{tenant}', 'i', 'mjcc', 'ref', '1', 'tester');
               INSERT INTO "{schema}".migration_id_map (tenant_id, migration_run_id, source_system, source_table, source_id, target_entity, target_id)
               VALUES ('{tenant}', '{run}', 'mjcc', 'inventory_items', '42', 'stock_item', gen_random_uuid());
               INSERT INTO "{schema}".migration_artifacts (tenant_id, migration_run_id, artifact_kind, path_or_object_ref, sha256, size_bytes, contains_customer_data)
               VALUES ('{tenant}', '{run}', 'report', 'r.json', 'h', 1, false);"#
        ))
        .unwrap();
    }
    let ids = |c: &mut ce_store_pg::Client, tenant: &str| -> Vec<String> {
        c.query(
            &format!(r#"SELECT target_id::text FROM "{schema}".migration_id_map WHERE tenant_id = $1::text::uuid AND source_id = '42'"#),
            &[&tenant],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
    };
    let (ia, ib) = (ids(&mut c, A), ids(&mut c, B));
    assert_eq!((ia.len(), ib.len()), (1, 1));
    assert_ne!(
        ia, ib,
        "the same legacy id maps to a different target per tenant"
    );
    let arts: i64 = c
        .query_one(
            &format!(r#"SELECT count(*) FROM "{schema}".migration_artifacts WHERE tenant_id = $1::text::uuid"#),
            &[&A],
        )
        .unwrap()
        .get(0);
    assert_eq!(arts, 1, "an artifact query scoped to A returns only A's");
    // Within one tenant the lineage is unique.
    let dup = c.batch_execute(&format!(
        r#"INSERT INTO "{schema}".migration_id_map (tenant_id, migration_run_id, source_system, source_table, source_id, target_entity, target_id)
           VALUES ('{A}', '{run_a}', 'mjcc', 'inventory_items', '42', 'stock_item', gen_random_uuid())"#
    ));
    assert!(
        dup.is_err(),
        "one source identity maps to one target per tenant"
    );
    drop(c);
    let _ = std::thread::spawn(move || drop(store)).join();
    let mut c = connect(&url, &Tls::Off).unwrap();
    let _ = c.batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#));
}
