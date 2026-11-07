//! Durable import jobs over HTTP, with the failures the invoice 511063 incident showed: a retry
//! that collides with half-written records, a lost response, a worker that dies after the commit,
//! a rule that fails late, two workers on one job.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_server::{router, run_next_job, AppState};
use ce_store::jobs::{Job, JobError, JobEvent, JobState, JobStore, NewJob};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
const INST: &str = "jb-1";

fn token(tenant: &str, role: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{role}","tenant":"{tenant}","roles":["{role}"],"exp":2000}}"#
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

fn manifest() -> Value {
    ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {
                "widget": {"fields": {
                    "name": {"type": "string", "required": true, "unique": true},
                    "qty": {"type": "integer"}
                }},
                "part": {"fields": {
                    "name": {"type": "string", "required": true},
                    "widget": {"type": "ref", "ref": "widget", "required": true, "indexed": true}
                }}
            },
            "roles": {
                "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
                "dropbox": {"read": [], "write": ["widget"], "actions": []}
            }
        }),
        &["audit"],
    )
    .unwrap()
}

/// Fails the first `complete`, as a worker killed between the commit and its bookkeeping would.
struct DropFirstComplete {
    inner: Arc<dyn JobStore>,
    armed: AtomicBool,
}

impl JobStore for DropFirstComplete {
    fn create(&self, n: &NewJob<'_>) -> Result<(Job, bool), JobError> {
        self.inner.create(n)
    }
    fn get(&self, t: &str, id: &str) -> Result<Job, JobError> {
        self.inner.get(t, id)
    }
    fn events(&self, t: &str, id: &str) -> Result<Vec<JobEvent>, JobError> {
        self.inner.events(t, id)
    }
    fn transition(
        &self,
        t: &str,
        id: &str,
        from: JobState,
        to: JobState,
        e: Option<(&str, &str)>,
    ) -> Result<Job, JobError> {
        self.inner.transition(t, id, from, to, e)
    }
    fn complete(&self, t: &str, id: &str, r: &Value) -> Result<Job, JobError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Err(JobError::LeaseLost);
        }
        self.inner.complete(t, id, r)
    }
    fn claim(&self, w: &str, l: i64, m: i32) -> Result<Option<Job>, JobError> {
        self.inner.claim(w, l, m)
    }
    fn heartbeat(
        &self,
        t: &str,
        id: &str,
        w: &str,
        l: i64,
        p: Option<i64>,
    ) -> Result<Job, JobError> {
        self.inner.heartbeat(t, id, w, l, p)
    }
    fn stage_rows(&self, t: &str, id: &str, rows: &[(i32, Value)]) -> Result<u64, JobError> {
        self.inner.stage_rows(t, id, rows)
    }
    fn staged(&self, t: &str, id: &str, a: i32, l: i64) -> Result<Vec<(i32, Value)>, JobError> {
        self.inner.staged(t, id, a, l)
    }
    fn staged_count(&self, t: &str, id: &str) -> Result<i64, JobError> {
        self.inner.staged_count(t, id)
    }
}

struct Rig {
    st: AppState<TypedPgStore>,
    app: Router,
    n: u32,
}

fn build(url: &str, jobs_on: bool, drop_first_complete: bool) -> Rig {
    let m = validate(manifest().to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_jb_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let jobs: Arc<dyn JobStore> = Arc::new(store.jobs().unwrap());
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = Arc::new(|| 1000);
    if jobs_on {
        let jobs: Arc<dyn JobStore> = if drop_first_complete {
            Arc::new(DropFirstComplete {
                inner: jobs,
                armed: AtomicBool::new(true),
            })
        } else {
            jobs
        };
        st = st.with_jobs(jobs);
    }
    Rig {
        app: router(st.clone()),
        st,
        n: 0,
    }
}

async fn rig(jobs_on: bool, drop_first_complete: bool) -> Option<Rig> {
    let url = url()?;
    Some(
        tokio::task::spawn_blocking(move || build(&url, jobs_on, drop_first_complete))
            .await
            .unwrap(),
    )
}

async fn close(r: Rig) {
    tokio::task::spawn_blocking(move || drop(r)).await.unwrap();
}

impl Rig {
    async fn call(
        &mut self,
        tenant: &str,
        role: &str,
        method: &str,
        path: &str,
        key: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.n += 1;
        let mut req = Request::builder()
            .method(method)
            .uri(format!("/v1/{INST}{path}"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(tenant, role)),
            );
        let auto = format!("auto{}", self.n);
        req = req.header("idempotency-key", key.unwrap_or(&auto));
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

    async fn own(&mut self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call(T1, "owner", method, path, None, body).await
    }

    async fn new_job(&mut self, key: &str, scope: &str, total: i64) -> Value {
        let (s, v) = self
            .call(
                T1,
                "owner",
                "POST",
                "/_jobs",
                Some(key),
                Some(json!({"kind": "invoice", "scope_key": scope, "total": total})),
            )
            .await;
        assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s} {v}");
        v["data"].clone()
    }

    async fn rows(&mut self, id: &str, start: i64, rows: Value) -> (StatusCode, Value) {
        self.own(
            "POST",
            &format!("/_jobs/{id}/rows"),
            Some(json!({"start": start, "rows": rows})),
        )
        .await
    }

    async fn list(&mut self, entity: &str) -> Vec<Value> {
        let (s, v) = self.own("GET", &format!("/{entity}?limit=100"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].as_array().unwrap().clone()
    }

    async fn state_of(&mut self, id: &str) -> Value {
        let (s, v) = self.own("GET", &format!("/_jobs/{id}"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].clone()
    }

    /// One worker pass.
    async fn work(&self, lease: i64) -> Option<ce_server::JobRun> {
        let st = self.st.clone();
        tokio::task::spawn_blocking(move || run_next_job(&st, "w1", lease, 3).unwrap())
            .await
            .unwrap()
    }
}

const W1: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const W2: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const P1: &str = "bbbbbbbb-0000-4000-8000-000000000001";

fn widget(id: &str, name: &str, qty: i64) -> Value {
    json!({"op": "create", "entity": "widget", "data": {"id": id, "name": name, "qty": qty}})
}

fn part(id: &str, name: &str, widget: &str) -> Value {
    json!({"op": "create", "entity": "part", "data": {"id": id, "name": name, "widget": widget}})
}

async fn queued_job(r: &mut Rig, key: &str, scope: &str, rows: Vec<Value>) -> String {
    let job = r.new_job(key, scope, rows.len() as i64).await;
    let id = job["id"].as_str().unwrap().to_string();
    let (s, v) = r.rows(&id, 0, Value::Array(rows)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["data"]["state"], "queued");
    id
}

#[tokio::test]
async fn a_job_posts_every_row_at_once_and_nothing_shows_before_that() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    let job = r.new_job("k1", "us/511063", 3).await;
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["state"], "uploaded");
    let rows = json!([
        widget(W1, "bolt", 5),
        widget(W2, "nut", 7),
        part(P1, "bolt-a", W1)
    ]);
    // The upload arrives in two pieces; the first piece is sent twice (a lost response).
    let (s, _) = r.rows(&id, 0, json!([rows[0], rows[1]])).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = r.rows(&id, 0, json!([rows[0], rows[1]])).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["data"]["staged_new"], 0, "a repeat adds nothing");
    let (s, _) = r.rows(&id, 2, json!([rows[2]])).await;
    assert_eq!(s, StatusCode::OK);
    // Staged rows are invisible to reads.
    assert!(r.list("widget").await.is_empty());
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    // A retried submit is harmless.
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["data"]["state"], "queued");
    assert!(
        r.list("widget").await.is_empty(),
        "nothing is posted until the worker commits"
    );

    let run = r.work(60).await.expect("a job was waiting");
    assert_eq!(
        (run.outcome, run.job_id.as_str()),
        ("completed", id.as_str())
    );
    let widgets = r.list("widget").await;
    assert_eq!(widgets.len(), 2);
    assert!(widgets.iter().any(|w| w["id"] == W1), "client ids are kept");
    assert_eq!(r.list("part").await.len(), 1);
    let done = r.state_of(&id).await;
    assert_eq!(done["state"], "completed");
    assert_eq!(done["result"]["by_entity"]["widget"], 2);
    let log: Vec<_> = done["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["to"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        log,
        [
            "uploaded",
            "parsing",
            "validated",
            "queued",
            "processing",
            "committing",
            "completed"
        ]
    );
    assert!(r.work(60).await.is_none(), "nothing left to do");
    close(r).await;
}

#[tokio::test]
async fn the_same_key_is_the_same_job_and_a_changed_request_is_refused() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    let a = r.new_job("k1", "s", 2).await;
    let b = r.new_job("k1", "s", 2).await;
    assert_eq!(a["id"], b["id"]);
    let (s, v) = r
        .call(
            T1,
            "owner",
            "POST",
            "/_jobs",
            Some("k1"),
            Some(json!({"kind": "invoice", "scope_key": "s", "total": 3})),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // Another live upload for the same scope is refused and names the one in progress.
    let (s, v) = r
        .call(
            T1,
            "owner",
            "POST",
            "/_jobs",
            Some("k2"),
            Some(json!({"kind": "invoice", "scope_key": "s", "total": 2})),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["job_id"], a["id"]);
    close(r).await;
}

#[tokio::test]
async fn a_clash_with_existing_records_is_found_at_submit_and_leaves_nothing() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    // The half-written state a failed browser import leaves behind.
    let (s, _) = r
        .own("POST", "/widget", Some(json!({"name": "bolt", "qty": 1})))
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let job = r.new_job("k1", "s", 2).await;
    let id = job["id"].as_str().unwrap().to_string();
    let (s, v) = r
        .rows(&id, 0, json!([widget(W2, "nut", 7), widget(W1, "bolt", 5)]))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert!(s.is_client_error(), "{s} {v}");
    let j = r.state_of(&id).await;
    assert_eq!(j["state"], "failed");
    assert!(j["error_code"].is_string());
    assert!(r.work(60).await.is_none(), "a failed job is never run");
    let widgets = r.list("widget").await;
    assert_eq!(
        widgets.len(),
        1,
        "nothing from the job was kept: {widgets:?}"
    );
    // The scope is free again; the corrected upload is generation 2.
    let again = r.new_job("k2", "s", 1).await;
    assert_eq!(again["generation"], 2);
    close(r).await;
}

#[tokio::test]
async fn a_worker_that_dies_after_the_commit_is_replayed_without_duplicates() {
    let Some(mut r) = rig(true, true).await else {
        return;
    };
    let id = queued_job(
        &mut r,
        "k1",
        "s",
        vec![widget(W1, "bolt", 5), part(P1, "bolt-a", W1)],
    )
    .await;
    let first = r.work(0).await.expect("claimed");
    assert_eq!(first.outcome, "retry", "{first:?}");
    // The rows are committed but the job is not marked done.
    assert_eq!(r.list("widget").await.len(), 1);
    assert_eq!(r.state_of(&id).await["state"], "committing");
    let audit_before = r.list("audit_event").await.len();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let second = r.work(60).await.expect("reclaimed");
    assert_eq!(second.outcome, "completed", "{second:?}");
    assert_eq!(r.list("widget").await.len(), 1, "no duplicate");
    assert_eq!(r.list("part").await.len(), 1);
    assert_eq!(
        r.list("audit_event").await.len(),
        audit_before,
        "a replay writes no second audit trail"
    );
    let done = r.state_of(&id).await;
    assert_eq!(done["state"], "completed");
    assert_eq!(done["attempt"], 2);
    close(r).await;
}

#[tokio::test]
async fn a_condition_that_fails_inside_the_commit_undoes_everything() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    // The condition is true at rehearsal only if the data is right; make it wrong.
    let job = r.new_job("k1", "s", 2).await;
    let id = job["id"].as_str().unwrap().to_string();
    let rows = json!([
        widget(W1, "bolt", 5),
        {"check": {"entity": "widget", "id": W1, "field": "qty", "equals": 6}}
    ]);
    let (s, v) = r.rows(&id, 0, rows).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert!(s.is_client_error(), "{s} {v}");
    let j = r.state_of(&id).await;
    assert_eq!(j["state"], "failed");
    assert_eq!(j["error_code"], "post_condition_failed");
    assert!(r.list("widget").await.is_empty());

    // A condition that holds lets the same shape through.
    let job = r.new_job("k2", "s", 2).await;
    let id = job["id"].as_str().unwrap().to_string();
    let rows = json!([
        widget(W1, "bolt", 5),
        {"check": {"entity": "widget", "id": W1, "field": "qty", "equals": 5}}
    ]);
    r.rows(&id, 0, rows).await;
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert_eq!(r.work(60).await.unwrap().outcome, "completed");
    assert_eq!(r.list("widget").await.len(), 1);
    close(r).await;
}

#[tokio::test]
async fn two_workers_racing_post_a_job_once() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    queued_job(
        &mut r,
        "k1",
        "s",
        vec![widget(W1, "bolt", 5), widget(W2, "nut", 7)],
    )
    .await;
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let st = r.st.clone();
            std::thread::spawn(move || run_next_job(&st, &format!("w{i}"), 60, 3).unwrap())
        })
        .collect();
    let ran: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert_eq!(ran[0].outcome, "completed");
    assert_eq!(r.list("widget").await.len(), 2);
    close(r).await;
}

#[tokio::test]
async fn a_cancelled_job_is_never_run_and_updates_are_not_accepted() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    let id = queued_job(&mut r, "k1", "s", vec![widget(W1, "bolt", 5)]).await;
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/cancel"), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["state"], "cancelled");
    assert!(r.work(60).await.is_none());
    assert!(r.list("widget").await.is_empty());

    let job = r.new_job("k2", "s", 1).await;
    let id = job["id"].as_str().unwrap().to_string();
    let (s, v) = r
        .rows(
            &id,
            0,
            json!([{"op": "update", "entity": "widget", "id": W1, "version": 1, "data": {"qty": 1}}]),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    close(r).await;
}

#[tokio::test]
async fn staging_enforces_the_callers_grants_and_the_tenant() {
    let Some(mut r) = rig(true, false).await else {
        return;
    };
    let job = r.new_job("k1", "s", 1).await;
    let id = job["id"].as_str().unwrap().to_string();
    // `dropbox` may write widgets but not parts, in a job of its own.
    let (s, v) = r
        .call(
            T1,
            "dropbox",
            "POST",
            "/_jobs",
            Some("kd"),
            Some(json!({"kind": "invoice", "scope_key": "d", "total": 1})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let did = v["data"]["id"].as_str().unwrap().to_string();
    let (s, _) = r
        .call(
            T1,
            "dropbox",
            "POST",
            &format!("/_jobs/{did}/rows"),
            None,
            Some(json!({"start": 0, "rows": [part(P1, "p", W1)]})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // A colleague who is not an admin cannot see, change or cancel it either.
    let (s, _) = r
        .call(T1, "dropbox", "GET", &format!("/_jobs/{id}"), None, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = r
        .call(
            T1,
            "dropbox",
            "POST",
            &format!("/_jobs/{id}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Another tenant cannot see the job.
    let (s, _) = r
        .call(T2, "owner", "GET", &format!("/_jobs/{id}"), None, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Submitting with rows missing is refused with the counts.
    let (s, v) = r.own("POST", &format!("/_jobs/{id}/submit"), None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["code"], "incomplete_upload");
    close(r).await;
}

#[tokio::test]
async fn without_the_feature_the_routes_say_so() {
    let Some(mut r) = rig(false, false).await else {
        return;
    };
    let (s, v) = r
        .own(
            "POST",
            "/_jobs",
            Some(json!({"kind": "invoice", "scope_key": "s", "total": 1})),
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["code"], "jobs_unavailable");
    close(r).await;
}
