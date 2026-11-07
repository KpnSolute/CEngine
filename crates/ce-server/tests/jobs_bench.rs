//! Benchmark for the durable import job against the batch path it replaces (CEngine #103).
//! Not run by default. Run with a database:
//!
//! ```text
//! CE_TEST_DATABASE_URL=... cargo test -p ce-server --test jobs_bench --release -- --ignored --nocapture
//! ```
//!
//! An invoice with a computed total is posted with N lines. The job path is staged (200 rows per
//! request), rehearsed at submit and committed by the worker in ONE transaction. The batch path is
//! the workspace's current bridge: 25 operations per request, one transaction each.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, run_next_job, AppState};
use ce_store::jobs::JobStore;
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::Arc;
use std::time::Instant;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "bench";

fn token() -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"o","tenant":"{T1}","roles":["owner"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "bench", "version": "0.1.0", "title": "Bench"},
            "entities": {
                "invoice": {"fields": {
                    "number": {"type": "string", "required": true},
                    "total": {"type": "decimal", "precision": 12, "scale": 2,
                              "computed": "sum(children(line, invoice).amount)"}
                }},
                "line": {"fields": {
                    "invoice": {"type": "ref", "ref": "invoice", "required": true, "indexed": true},
                    "amount": {"type": "decimal", "precision": 12, "scale": 2}
                }}
            },
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        }),
        &["audit"],
    )
    .unwrap()
}

fn id(group: u32, i: u32) -> String {
    format!("{group:08x}-0000-4000-8000-{i:012x}")
}

fn rows(group: u32, n: u32) -> Vec<Value> {
    let inv = id(group, 0);
    let mut v = vec![
        json!({"op": "create", "entity": "invoice", "data": {"id": inv, "number": format!("N{group}")}}),
    ];
    for i in 1..=n {
        v.push(json!({"op": "create", "entity": "line",
            "data": {"id": id(group, i), "invoice": inv, "amount": "1.50"}}));
    }
    v
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(format!("/v1/{INST}{path}"))
        .header(header::AUTHORIZATION, format!("Bearer {}", token()))
        .header("idempotency-key", key);
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
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
    let s = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run explicitly"]
async fn job_versus_batch() {
    let Ok(url) = std::env::var("CE_TEST_DATABASE_URL") else {
        eprintln!("skipping: CE_TEST_DATABASE_URL not set");
        return;
    };
    let st = tokio::task::spawn_blocking(move || {
        let m = validate(manifest().to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
        let schema = format!("ce_bench_{}", std::process::id());
        let store = TypedPgStore::install(&url, &m, &schema).unwrap();
        let jobs: Arc<dyn JobStore> = Arc::new(store.jobs().unwrap());
        let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
        st.now = Arc::new(|| 1000);
        st.with_jobs(jobs)
    })
    .await
    .unwrap();
    let app = router(st.clone());

    println!(
        "\nlines | stage ms | submit(rehearse) ms | commit ms p50/p95 | job total ms p50/p95 | batch 25/req ms | requests job/batch"
    );
    let mut group = 1u32;
    for n in [50u32, 200, 500, 1000] {
        let (mut commits, mut totals) = (Vec::new(), Vec::new());
        let (mut stage_ms, mut submit_ms, mut batch_ms) = (0.0, 0.0, 0.0);
        for rep in 0..5 {
            let all = rows(group, n);
            let t0 = Instant::now();
            let body =
                json!({"kind": "invoice", "scope_key": format!("s{group}"), "total": all.len()});
            let (s, v) = call(&app, "POST", "/_jobs", &format!("j{group}"), Some(body)).await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
            let job_id = v["data"]["id"].as_str().unwrap().to_string();
            for (c, chunk) in all.chunks(200).enumerate() {
                let body = json!({"start": c * 200, "rows": chunk});
                let (s, v) = call(
                    &app,
                    "POST",
                    &format!("/_jobs/{job_id}/rows"),
                    "x",
                    Some(body),
                )
                .await;
                assert_eq!(s, StatusCode::OK, "{v}");
            }
            let staged = ms(t0);
            let t1 = Instant::now();
            let (s, v) = call(&app, "POST", &format!("/_jobs/{job_id}/submit"), "x", None).await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
            let submit = ms(t1);
            let t2 = Instant::now();
            let st2 = st.clone();
            let run =
                tokio::task::spawn_blocking(move || run_next_job(&st2, "bench", 600, 3).unwrap())
                    .await
                    .unwrap()
                    .expect("job ran");
            assert_eq!(run.outcome, "completed", "{run:?}");
            commits.push(ms(t2));
            totals.push(ms(t0));
            if rep == 0 {
                stage_ms = staged;
                submit_ms = submit;
            }
            group += 1;

            if rep == 0 {
                let all = rows(group, n);
                let t3 = Instant::now();
                for (c, chunk) in all.chunks(25).enumerate() {
                    let (s, v) = call(
                        &app,
                        "POST",
                        "/_batch",
                        &format!("b{group}-{c}"),
                        Some(json!({"ops": chunk})),
                    )
                    .await;
                    assert_eq!(s, StatusCode::OK, "{v}");
                }
                batch_ms = ms(t3);
                group += 1;
            }
        }
        println!(
            "{n:5} | {stage_ms:8.0} | {submit_ms:19.0} | {:8.0} / {:<7.0} | {:10.0} / {:<7.0} | {batch_ms:15.0} | {} / {}",
            pct(&mut commits.clone(), 0.5),
            pct(&mut commits, 0.95),
            pct(&mut totals.clone(), 0.5),
            pct(&mut totals, 0.95),
            (n as usize + 1).div_ceil(200) + 3,
            (n as usize + 1).div_ceil(25),
        );
    }
    tokio::task::spawn_blocking(move || drop((st, app)))
        .await
        .unwrap();
}
