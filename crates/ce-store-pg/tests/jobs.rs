//! Durable import jobs on a real Postgres (`CE_TEST_DATABASE_URL`; CI sets `CE_REQUIRE_DB`).
use ce_store_pg::{ImportJobs, JobError, JobState, NewJob, TypedPgStore};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";

static N: AtomicU32 = AtomicU32::new(0);

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

fn fresh(url: &str) -> (TypedPgStore, ImportJobs) {
    let m = json!({
      "manifest_version": 1,
      "template": {"name": "jobs_test", "version": "0.1.0", "title": "Jobs test"},
      "entities": {"item": {"fields": {"n": {"type": "integer"}}}}
    });
    let m = ce_manifest::validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let name = format!(
        "ce_jobs_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &name).expect("install");
    let jobs = store.jobs().expect("jobs");
    (store, jobs)
}

fn new<'a>(tenant: &'a str, key: &'a str, scope: &'a str) -> NewJob<'a> {
    NewJob {
        tenant,
        kind: "invoice",
        scope_key: scope,
        idempotency_key: key,
        fingerprint: "fp-1",
        file_sha256: Some("abc"),
        upload_ref: Some("uploads/abc"),
        actor: "tester",
        actor_role: Some("manager"),
        total: 502,
    }
}

/// Moves a fresh job to `queued`.
fn queue(jobs: &ImportJobs, tenant: &str, id: &str) {
    for (a, b) in [
        (JobState::Uploaded, JobState::Parsing),
        (JobState::Parsing, JobState::Validated),
        (JobState::Validated, JobState::Queued),
    ] {
        jobs.transition(tenant, id, a, b, None).expect("transition");
    }
}

#[test]
fn the_same_key_returns_the_same_job_and_a_changed_request_is_refused() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, made) = jobs.create(&new(T1, "k1", "us/511063")).unwrap();
    assert!(made);
    assert_eq!(a.state, JobState::Uploaded);
    assert_eq!(a.generation, 1);
    let (b, made) = jobs.create(&new(T1, "k1", "us/511063")).unwrap();
    assert!(!made);
    assert_eq!(b.id, a.id);
    let mut other = new(T1, "k1", "us/511063");
    other.fingerprint = "fp-2";
    assert_eq!(jobs.create(&other), Err(JobError::IdempotencyKeyReused));
    store.drop_schema().unwrap();
}

#[test]
fn one_live_job_per_scope_and_a_reupload_is_a_new_generation() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "us/511063")).unwrap();
    // A different key for the same scope while the first is live: refused, naming the live job.
    assert_eq!(
        jobs.create(&new(T1, "k2", "us/511063")),
        Err(JobError::ScopeBusy {
            job_id: a.id.clone()
        })
    );
    // Another tenant may use the same scope key.
    jobs.create(&new(T2, "k1", "us/511063")).unwrap();
    // After the first ends, a re-upload is generation 2, and generation 1 stays in history.
    jobs.transition(T1, &a.id, JobState::Uploaded, JobState::Cancelled, None)
        .unwrap();
    let (b, made) = jobs.create(&new(T1, "k2", "us/511063")).unwrap();
    assert!(made);
    assert_eq!(b.generation, 2);
    assert_eq!(jobs.get(T1, &a.id).unwrap().state, JobState::Cancelled);
    store.drop_schema().unwrap();
}

#[test]
fn racing_creates_for_one_scope_make_exactly_one_job() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let jobs = Arc::new(jobs);
    let handles: Vec<_> = (0..6)
        .map(|i| {
            let jobs = jobs.clone();
            std::thread::spawn(move || {
                let key = format!("race-{i}");
                jobs.create(&new(T1, &key, "us/511063"))
                    .map(|(_, made)| made)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let made = results.iter().filter(|r| matches!(r, Ok(true))).count();
    let busy = results
        .iter()
        .filter(|r| matches!(r, Err(JobError::ScopeBusy { .. })))
        .count();
    assert_eq!((made, busy), (1, 5), "{results:?}");
    store.drop_schema().unwrap();
}

#[test]
fn transitions_are_checked_and_compare_and_set() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    assert_eq!(
        jobs.transition(T1, &a.id, JobState::Uploaded, JobState::Completed, None),
        Err(JobError::BadTransition {
            from: JobState::Uploaded,
            to: JobState::Completed
        })
    );
    jobs.transition(T1, &a.id, JobState::Uploaded, JobState::Parsing, None)
        .unwrap();
    // A writer that still thinks the job is `uploaded` loses.
    assert_eq!(
        jobs.transition(T1, &a.id, JobState::Uploaded, JobState::Cancelled, None),
        Err(JobError::StateChanged {
            actual: JobState::Parsing
        })
    );
    // Another tenant cannot see or move it.
    assert_eq!(jobs.get(T2, &a.id), Err(JobError::NotFound));
    assert_eq!(
        jobs.transition(T2, &a.id, JobState::Parsing, JobState::Validated, None),
        Err(JobError::NotFound)
    );
    let log: Vec<_> = jobs
        .events(T1, &a.id)
        .unwrap()
        .into_iter()
        .map(|e| e.to_state)
        .collect();
    assert_eq!(log, ["uploaded", "parsing"]);
    store.drop_schema().unwrap();
}

#[test]
fn staging_is_repeatable_and_closed_once_the_job_leaves_parsing() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    let rows: Vec<_> = (0..5).map(|i| (i, json!({"line": i}))).collect();
    assert_eq!(jobs.stage_rows(T1, &a.id, &rows).unwrap(), 5);
    // A repeat (a retried request) adds nothing and changes nothing.
    assert_eq!(jobs.stage_rows(T1, &a.id, &rows).unwrap(), 0);
    assert_eq!(jobs.staged_count(T1, &a.id).unwrap(), 5);
    let page = jobs.staged(T1, &a.id, 1, 2).unwrap();
    assert_eq!(page.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(jobs.staged_count(T2, &a.id).unwrap(), 0, "tenant isolation");
    // The same position with different content is refused, not silently ignored.
    assert_eq!(
        jobs.stage_rows(T1, &a.id, &[(0, json!({"line": 99}))]),
        Err(JobError::StagedMismatch { seq: 0 })
    );
    queue(&jobs, T1, &a.id);
    assert_eq!(
        jobs.stage_rows(T1, &a.id, &[(9, json!({}))]),
        Err(JobError::StateChanged {
            actual: JobState::Queued
        })
    );
    store.drop_schema().unwrap();
}

#[test]
fn two_workers_cannot_claim_one_job() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    queue(&jobs, T1, &a.id);
    let jobs = Arc::new(jobs);
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let jobs = jobs.clone();
            std::thread::spawn(move || jobs.claim(&format!("w{i}"), 60, 3).unwrap())
        })
        .collect();
    let won: Vec<_> = handles
        .into_iter()
        .filter_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(won.len(), 1);
    assert_eq!(won[0].state, JobState::Processing);
    assert_eq!(won[0].attempt, 1);
    store.drop_schema().unwrap();
}

#[test]
fn a_stalled_worker_loses_the_job_and_the_new_worker_continues() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    queue(&jobs, T1, &a.id);
    let first = jobs.claim("w1", 0, 3).unwrap().expect("claimed");
    std::thread::sleep(std::time::Duration::from_millis(50));
    // The lease ran out, so w1's heartbeat fails and w2 takes over as attempt 2.
    assert_eq!(
        jobs.heartbeat(T1, &first.id, "w1", 60, Some(10)),
        Err(JobError::LeaseLost)
    );
    let second = jobs.claim("w2", 60, 3).unwrap().expect("reclaimed");
    assert_eq!((second.id.as_str(), second.attempt), (first.id.as_str(), 2));
    assert_eq!(
        jobs.heartbeat(T1, &first.id, "w1", 60, None),
        Err(JobError::LeaseLost),
        "the old worker stays locked out"
    );
    let beat = jobs.heartbeat(T1, &first.id, "w2", 60, Some(250)).unwrap();
    assert_eq!(beat.processed, 250);
    store.drop_schema().unwrap();
}

#[test]
fn a_job_that_keeps_stalling_fails_instead_of_looping_forever() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    queue(&jobs, T1, &a.id);
    for attempt in 1..=2 {
        let j = jobs.claim("w", 0, 2).unwrap().expect("claimed");
        assert_eq!(j.attempt, attempt);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(jobs.claim("w", 0, 2).unwrap(), None);
    let done = jobs.get(T1, &a.id).unwrap();
    assert_eq!(done.state, JobState::Failed);
    assert_eq!(done.error_code.as_deref(), Some("attempts_exhausted"));
    assert!(done.completed_at.is_some());
    // The scope is free again for a fresh attempt.
    let (b, made) = jobs.create(&new(T1, "k2", "s")).unwrap();
    assert!(made);
    assert_eq!(b.generation, 2);
    store.drop_schema().unwrap();
}

#[test]
fn a_job_completes_through_committing_with_its_result() {
    let Some(u) = url() else { return };
    let (store, jobs) = fresh(&u);
    let (a, _) = jobs.create(&new(T1, "k1", "s")).unwrap();
    queue(&jobs, T1, &a.id);
    let j = jobs.claim("w1", 60, 3).unwrap().unwrap();
    jobs.transition(T1, &j.id, JobState::Processing, JobState::Committing, None)
        .unwrap();
    // A job that is committing cannot be cancelled.
    assert_eq!(
        jobs.transition(T1, &j.id, JobState::Committing, JobState::Cancelled, None),
        Err(JobError::BadTransition {
            from: JobState::Committing,
            to: JobState::Cancelled
        })
    );
    let done = jobs.complete(T1, &j.id, &json!({"receipts": 474})).unwrap();
    assert_eq!(done.state, JobState::Completed);
    assert_eq!(done.processed, done.total);
    assert_eq!(done.result, Some(json!({"receipts": 474})));
    assert_eq!(done.lease_owner, None);
    // Completing twice is a state conflict, not a second result.
    assert_eq!(
        jobs.complete(T1, &j.id, &json!({})),
        Err(JobError::StateChanged {
            actual: JobState::Completed
        })
    );
    store.drop_schema().unwrap();
}
