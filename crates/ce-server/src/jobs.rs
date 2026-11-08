//! Durable import jobs over HTTP (CEngine #103, phase 2), and the worker that posts them.
//!
//! A long import (a 13-page supplier invoice is about 500 writes) is one job the server owns, not
//! a series of browser requests whose progress only the tab remembers.
//!
//! ```text
//! POST /v1/{inst}/_jobs              {kind, scope_key, total}  + Idempotency-Key  -> job (uploaded)
//! POST /v1/{inst}/_jobs/{id}/rows    {start, rows: [...]}      repeatable         -> rows staged
//! POST /v1/{inst}/_jobs/{id}/submit                            repeatable         -> queued
//! GET  /v1/{inst}/_jobs/{id}                                                      -> status + log
//! POST /v1/{inst}/_jobs/{id}/cancel
//! ```
//!
//! Rows are `create` operations in the `_batch` shape, or `{"check": {entity, id, field, equals}}`
//! conditions. They are authorised and validated as the caller stages them, kept in a table no
//! entity read can see, and rehearsed in a rolled-back transaction when the job is submitted, so a
//! primary-key clash or a broken rule is reported to the person at the screen, not after they left.
//!
//! The worker (`run_next`) leases a queued job and posts every row in ONE transaction, then
//! evaluates the checks inside that same transaction. Either everything lands and the job is
//! `completed`, or nothing is kept. Every create carries the idempotency key `job:{id}:{seq}`, so a
//! worker that died after the commit and is reclaimed replays harmlessly to the same result.
//! Updates and deletes are not accepted: they cannot be replayed safely.

use super::{
    authenticate, body_problem, changesets, check_server_only, failure_problem, finish,
    idempotency_key, invariant_context, parse_object, prepare_create, request_id, store_problem,
    take_client_id, validate_body, writes, AppState, Problem,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use ce_auth::Principal;
use ce_policy::Access;
use ce_store::jobs::{Job, JobError, JobEvent, JobState, JobStore, NewJob};
use ce_store::Store;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// The most rows one job may hold.
pub const MAX_JOB_ROWS: i64 = 5000;
/// The most rows one staging request may carry.
const MAX_ROWS_PER_REQUEST: usize = 200;
const PAGE: i64 = 500;

fn unavailable() -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "jobs_unavailable",
        "durable import jobs are not turned on for this installation",
    )
}

fn job_problem(e: JobError) -> Problem {
    match e {
        JobError::NotFound => Problem::not_found(),
        JobError::Store(e) => store_problem(e),
        JobError::IdempotencyKeyReused => Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency_key_reused",
            "this Idempotency-Key was already used for a different job",
        ),
        JobError::ScopeBusy { job_id } => {
            let mut p = Problem::new(
                StatusCode::CONFLICT,
                "scope_busy",
                "another import for this scope is still in progress",
            );
            p.extra.push(("job_id", json!(job_id)));
            p
        }
        JobError::BadTransition { from, to } => Problem::new(
            StatusCode::CONFLICT,
            "bad_transition",
            format!(
                "a job cannot move from {} to {}",
                from.as_str(),
                to.as_str()
            ),
        ),
        JobError::StateChanged { actual } => {
            let mut p = Problem::new(
                StatusCode::CONFLICT,
                "job_state_changed",
                format!("the job is now {}", actual.as_str()),
            );
            p.extra.push(("state", json!(actual.as_str())));
            p
        }
        JobError::LeaseLost => Problem::new(
            StatusCode::CONFLICT,
            "lease_lost",
            "the worker no longer holds this job",
        ),
        JobError::StagedMismatch { seq } => Problem::new(
            StatusCode::CONFLICT,
            "staged_mismatch",
            format!("row {seq} was already staged with different content"),
        ),
    }
}

fn job_json(j: &Job) -> Value {
    json!({
        "id": j.id,
        "kind": j.kind,
        "scope_key": j.scope_key,
        "generation": j.generation,
        "state": j.state.as_str(),
        "total": j.total,
        "processed": j.processed,
        "attempt": j.attempt,
        "actor": j.actor,
        "error_code": j.error_code,
        "error_message": j.error_message,
        "result": j.result,
        "file_sha256": j.file_sha256,
        "upload_ref": j.upload_ref,
        "created_at": j.created_at,
        "updated_at": j.updated_at,
        "completed_at": j.completed_at,
    })
}

fn event_json(e: &JobEvent) -> Value {
    json!({"from": e.from_state, "to": e.to_state, "detail": e.detail, "at": e.at})
}

fn envelope(status: StatusCode, rid: &str, data: Value) -> Response {
    (
        status,
        Json(json!({"data": data, "meta": {"request_id": rid}})),
    )
        .into_response()
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Problem> + Send + 'static,
) -> Result<T, Problem> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| store_problem(ce_store::StoreError::Backend("task failed".into())))?
}

/// A job belongs to whoever started it; the staff-admin role may act on any job. Anyone else is
/// told it does not exist, so job ids do not leak between colleagues.
fn may_touch<S>(st: &AppState<S>, who: &Principal, job: &Job) -> Result<(), Problem> {
    if job.actor == who.user_id || who.has_role(&st.staff_admin_role) {
        Ok(())
    } else {
        Err(Problem::not_found())
    }
}

fn store_of<S>(st: &AppState<S>) -> Result<Arc<dyn JobStore>, Problem> {
    st.jobs.clone().ok_or_else(unavailable)
}

fn bad_field(path: &str, why: &str) -> Problem {
    Problem::validation(
        format!("{path}: {why}"),
        vec![json!({"path": path, "code": "invalid", "detail": why})],
    )
}

fn authed<S>(st: &AppState<S>, headers: &HeaderMap, inst: &str) -> Result<Principal, Problem> {
    let who = authenticate(st, headers)?;
    if inst != st.installation {
        return Err(Problem::not_found());
    }
    Ok(who)
}

// ----- rows -----------------------------------------------------------------------------------

enum Row {
    Write(String, writes::Op),
    Check(writes::Check),
}

/// Reads one staged row. With `who` the caller's grants are enforced (staging time); without it the
/// row is rebuilt for the worker, which trusts what staging accepted.
fn parse_row<S>(
    st: &AppState<S>,
    who: Option<&Principal>,
    seq: i64,
    item: &Value,
    prefix: &str,
) -> Result<Row, Problem> {
    let bad = |what: &str| {
        Problem::validation(
            format!("row {seq}: {what}"),
            vec![json!({"path": format!("/rows/{seq}"), "code": "invalid_row", "detail": what})],
        )
    };
    if let Some(c) = item.get("check") {
        let s = |k: &str| c[k].as_str().map(String::from);
        let (Some(entity), Some(id), Some(field)) = (s("entity"), s("id"), s("field")) else {
            return Err(bad("a check needs entity, id and field"));
        };
        if c.get("equals").is_none() {
            return Err(bad("a check needs equals"));
        }
        if !st.manifest.value["entities"][entity.as_str()].is_object() {
            return Err(bad("unknown entity"));
        }
        if let Some(who) = who {
            st.policy
                .authorize(who, Access::Read, &entity)
                .map_err(|_| Problem::forbidden())?;
        }
        return Ok(Row::Check(writes::Check {
            entity,
            id,
            field,
            equals: c["equals"].clone(),
        }));
    }
    if item["op"].as_str() != Some("create") {
        return Err(bad(
            "a job accepts create operations and checks only; updates and deletes cannot be replayed safely",
        ));
    }
    let entity = item["entity"]
        .as_str()
        .ok_or_else(|| bad("needs an entity"))?;
    if !st.manifest.value["entities"][entity].is_object() {
        return Err(bad("unknown entity"));
    }
    if let Some(who) = who {
        st.policy
            .authorize(who, Access::Write, entity)
            .map_err(|_| Problem::forbidden())?;
        let can_merge = st.policy.can_run(&who.roles, changesets::MERGE_ACTION);
        if changesets::governed(&st.manifest, entity) && !can_merge {
            return Err(bad(
                "this entity is governed; propose the change as a changeset",
            ));
        }
    }
    let mut obj = item["data"]
        .as_object()
        .cloned()
        .ok_or_else(|| bad("create needs data"))?;
    let chosen = take_client_id(&mut obj);
    let check = obj.clone();
    prepare_create(&st.manifest, entity, &mut obj);
    if let Some(id) = chosen {
        obj.insert("id".into(), id);
    }
    validate_body(st, entity, &check, ce_validate::Mode::Create)?;
    let op = writes::Op::Create {
        data: obj,
        key: format!("{prefix}:{seq}"),
    };
    check_server_only(&st.manifest, entity, &op)?;
    Ok(Row::Write(entity.to_string(), op))
}

fn all_staged(jobs: &dyn JobStore, tenant: &str, id: &str) -> Result<Vec<(i32, Value)>, JobError> {
    let mut out = Vec::new();
    let mut after = -1;
    loop {
        let page = jobs.staged(tenant, id, after, PAGE)?;
        let Some(last) = page.last().map(|(s, _)| *s) else {
            return Ok(out);
        };
        after = last;
        let full = page.len() as i64 == PAGE;
        out.extend(page);
        if !full {
            return Ok(out);
        }
    }
}

type Built = (Vec<(String, writes::Op)>, Vec<writes::Check>);

fn build<S>(st: &AppState<S>, jobs: &dyn JobStore, job: &Job) -> Result<Built, Problem> {
    let prefix = format!("job:{}", job.id);
    let staged = all_staged(jobs, &job.tenant_id, &job.id).map_err(job_problem)?;
    let (mut ops, mut checks) = (Vec::new(), Vec::new());
    for (seq, item) in staged {
        match parse_row(st, None, i64::from(seq), &item, &prefix)? {
            Row::Write(e, op) => ops.push((e, op)),
            Row::Check(c) => checks.push(c),
        }
    }
    Ok((ops, checks))
}

/// A short stable code and a message for a failed post. `true` when trying again could succeed.
fn classify(f: &writes::Failure) -> (&'static str, String, bool) {
    use ce_store::StoreError as E;
    match f {
        writes::Failure::Rules(errors) => (
            if errors.iter().any(|e| e.code == "post_condition_failed") {
                "post_condition_failed"
            } else {
                "rules"
            },
            errors
                .iter()
                .take(3)
                .map(|e| format!("{}: {}", e.path, e.detail))
                .collect::<Vec<_>>()
                .join("; "),
            false,
        ),
        writes::Failure::Locked => (
            "locked",
            "a record is frozen; reopen its period".into(),
            false,
        ),
        writes::Failure::Store(E::Backend(_)) => (
            "backend_unavailable",
            "the database did not answer".into(),
            true,
        ),
        writes::Failure::Store(E::Constraint(kind)) => (
            "constraint",
            format!("the data breaks a database rule ({kind})"),
            false,
        ),
        writes::Failure::Store(e) => ("rejected", format!("{e:?}"), false),
    }
}

// ----- handlers -------------------------------------------------------------------------------

/// `POST .../_jobs`
pub(crate) async fn create<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let key = idempotency_key(&headers)?;
        let body = parse_object(&body.map_err(body_problem)?)?;
        let text = |k: &str| body.get(k).and_then(Value::as_str).map(String::from);
        let kind = text("kind").ok_or_else(|| bad_field("/kind", "required"))?;
        if kind.is_empty()
            || kind.len() > 64
            || !kind.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.')
            })
        {
            return Err(bad_field("/kind", "1 to 64 of a-z, 0-9, _ - ."));
        }
        let scope_key = text("scope_key").ok_or_else(|| bad_field("/scope_key", "required"))?;
        if scope_key.is_empty() || scope_key.len() > 200 {
            return Err(bad_field("/scope_key", "1 to 200 characters"));
        }
        let total = body
            .get("total")
            .and_then(Value::as_i64)
            .filter(|t| (1..=MAX_JOB_ROWS).contains(t))
            .ok_or_else(|| bad_field("/total", &format!("a number from 1 to {MAX_JOB_ROWS}")))?;
        let (file_sha256, upload_ref) = (text("file_sha256"), text("upload_ref"));
        let fingerprint = crate::files::sha256_hex(
            json!([kind, scope_key, total, file_sha256, upload_ref])
                .to_string()
                .as_bytes(),
        );
        let tenant = who.scope().tenant_id().to_string();
        let (user, role) = (who.user_id.clone(), who.roles.first().cloned());
        let (job, made) = blocking(move || {
            jobs.create(&NewJob {
                tenant: &tenant,
                kind: &kind,
                scope_key: &scope_key,
                idempotency_key: &key,
                fingerprint: &fingerprint,
                file_sha256: file_sha256.as_deref(),
                upload_ref: upload_ref.as_deref(),
                actor: &user,
                actor_role: role.as_deref(),
                total,
            })
            .map_err(job_problem)
        })
        .await?;
        let status = if made {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok(envelope(status, &rid, job_json(&job)))
    };
    finish(&rid, run.await)
}

/// `GET .../_jobs/{id}`
pub(crate) async fn status<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let (job, events, staged) = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            let events = jobs.events(&tenant, &id).map_err(job_problem)?;
            let staged = jobs.staged_count(&tenant, &id).map_err(job_problem)?;
            Ok((job, events, staged))
        })
        .await?;
        let mut data = job_json(&job);
        data["staged"] = json!(staged);
        data["events"] = Value::Array(events.iter().map(event_json).collect());
        Ok(envelope(StatusCode::OK, &rid, data))
    };
    finish(&rid, run.await)
}

/// `POST .../_jobs/{id}/rows` with `{"start": 0, "rows": [...]}`
pub(crate) async fn stage<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let body = parse_object(&body.map_err(body_problem)?)?;
        let start = body
            .get("start")
            .and_then(Value::as_i64)
            .filter(|s| *s >= 0)
            .ok_or_else(|| bad_field("/start", "the position of the first row, from 0"))?;
        let rows = body
            .get("rows")
            .and_then(Value::as_array)
            .filter(|r| !r.is_empty() && r.len() <= MAX_ROWS_PER_REQUEST)
            .ok_or_else(|| {
                bad_field(
                    "/rows",
                    &format!("1 to {MAX_ROWS_PER_REQUEST} rows per request"),
                )
            })?
            .clone();
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let (job, added) = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            if start + rows.len() as i64 > job.total {
                return Err(bad_field("/rows", "more rows than the job's total"));
            }
            let prefix = format!("job:{}", job.id);
            let mut staged = Vec::with_capacity(rows.len());
            for (i, item) in rows.iter().enumerate() {
                let seq = start + i as i64;
                parse_row(&st2, Some(&who), seq, item, &prefix)?;
                staged.push((seq as i32, item.clone()));
            }
            if job.state == JobState::Uploaded {
                // A concurrent first call may have done it already; staging checks the state.
                let _ = jobs.transition(&tenant, &id, JobState::Uploaded, JobState::Parsing, None);
            }
            let added = jobs
                .stage_rows(&tenant, &id, &staged)
                .map_err(job_problem)?;
            Ok((job, added))
        })
        .await?;
        Ok(envelope(
            StatusCode::OK,
            &rid,
            json!({"job_id": job.id, "staged_new": added}),
        ))
    };
    finish(&rid, run.await)
}

/// `POST .../_jobs/{id}/submit`: rehearse the whole post, then queue it. Safe to repeat.
pub(crate) async fn submit<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let rid2 = rid.clone();
        let job = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            match job.state {
                JobState::Uploaded | JobState::Parsing | JobState::NeedsReview => {}
                JobState::Failed | JobState::Cancelled => {
                    return Err(job_problem(JobError::StateChanged { actual: job.state }))
                }
                // Already submitted: a retried submit gets the current state back.
                _ => return Ok(job),
            }
            let have = jobs.staged_count(&tenant, &id).map_err(job_problem)?;
            if have != job.total {
                let mut p = Problem::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "incomplete_upload",
                    format!("{have} of {} rows are staged", job.total),
                );
                p.extra.push(("staged", json!(have)));
                p.extra.push(("total", json!(job.total)));
                return Err(p);
            }
            let from = if job.state == JobState::Uploaded {
                jobs.transition(&tenant, &id, JobState::Uploaded, JobState::Parsing, None)
                    .map_err(job_problem)?;
                JobState::Parsing
            } else {
                job.state
            };
            let fail = |code: &str, msg: &str| {
                let _ = jobs.transition(&tenant, &id, from, JobState::Failed, Some((code, msg)));
            };
            let (ops, checks) = match build(&st2, &*jobs, &job) {
                Ok(b) => b,
                Err(p) => {
                    if p.status.is_client_error() {
                        fail("bad_rows", &p.detail);
                    }
                    return Err(p);
                }
            };
            let ctx = invariant_context(&st2, &who);
            let scope = who.scope().clone();
            let env = writes::Env {
                manifest: &st2.manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            match writes::execute_all_checked(&*st2.store, &env, &ops, &checks, true) {
                Ok(_) => {}
                Err(f) => {
                    let (code, msg, retry) = classify(&f);
                    if !retry {
                        fail(code, &msg);
                    }
                    return Err(failure_problem(f));
                }
            }
            jobs.transition(&tenant, &id, from, JobState::Validated, None)
                .map_err(job_problem)?;
            jobs.transition(&tenant, &id, JobState::Validated, JobState::Queued, None)
                .map_err(job_problem)
        })
        .await?;
        Ok(envelope(StatusCode::ACCEPTED, &rid, job_json(&job)))
    };
    finish(&rid, run.await)
}

/// `POST .../_jobs/{id}/cancel`
pub(crate) async fn cancel<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let job = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            if job.state == JobState::Cancelled {
                return Ok(job);
            }
            jobs.transition(&tenant, &id, job.state, JobState::Cancelled, None)
                .map_err(job_problem)
        })
        .await?;
        Ok(envelope(StatusCode::OK, &rid, job_json(&job)))
    };
    finish(&rid, run.await)
}

/// `GET .../_jobs?limit=100&mine=true`: newest first. The staff-admin role sees every job in the
/// installation unless it asks for its own; anyone else sees only the jobs they started.
pub(crate) async fn list<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let limit = q
            .get("limit")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(100)
            .clamp(1, 500);
        let mine = q.get("mine").is_some_and(|v| v == "true" || v == "1");
        let actor = if mine || !who.has_role(&st.staff_admin_role) {
            Some(who.user_id.clone())
        } else {
            None
        };
        let rows = blocking(move || {
            jobs.list(&tenant, actor.as_deref(), limit)
                .map_err(job_problem)
        })
        .await?;
        Ok(envelope(
            StatusCode::OK,
            &rid,
            Value::Array(rows.iter().map(job_json).collect()),
        ))
    };
    finish(&rid, run.await)
}

/// Moves a job the caller may touch from one of `from` to `to`. Repeating it once the job is
/// already in `to` returns the job unchanged.
async fn hold<S: Store + Send + Sync + 'static>(
    st: AppState<S>,
    inst: String,
    id: String,
    headers: HeaderMap,
    from: &'static [JobState],
    to: JobState,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let job = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            if job.state == to {
                return Ok(job);
            }
            if !from.contains(&job.state) {
                return Err(job_problem(JobError::StateChanged { actual: job.state }));
            }
            jobs.transition(&tenant, &id, job.state, to, None)
                .map_err(job_problem)
        })
        .await?;
        Ok(envelope(StatusCode::OK, &rid, job_json(&job)))
    };
    finish(&rid, run.await)
}

/// `POST .../_jobs/{id}/pause`: holds a queued or retryable job so no worker picks it up.
pub(crate) async fn pause<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    hold(
        st,
        inst,
        id,
        headers,
        &[JobState::Queued, JobState::Retryable],
        JobState::Paused,
    )
    .await
}

/// `POST .../_jobs/{id}/resume`: queues a paused job again.
pub(crate) async fn resume<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    hold(st, inst, id, headers, &[JobState::Paused], JobState::Queued).await
}

/// `POST .../_jobs/{id}/restart`: a failed or cancelled job runs again as its next generation,
/// with the same staged rows. The new job is returned in `uploaded`; submit it to rehearse and
/// queue it. Safe to repeat.
pub(crate) async fn restart<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authed(&st, &headers, &inst)?;
        let jobs = store_of(&st)?;
        let tenant = who.scope().tenant_id().to_string();
        let st2 = st.clone();
        let job = blocking(move || {
            let job = jobs.get(&tenant, &id).map_err(job_problem)?;
            may_touch(&st2, &who, &job)?;
            jobs.restart(&tenant, &id, &who.user_id)
                .map_err(job_problem)
        })
        .await?;
        Ok(envelope(StatusCode::CREATED, &rid, job_json(&job)))
    };
    finish(&rid, run.await)
}

// ----- worker ---------------------------------------------------------------------------------

/// What one pass of the worker did.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRun {
    pub job_id: String,
    pub tenant_id: String,
    /// `completed`, `failed` or `retry`.
    pub outcome: &'static str,
    pub detail: String,
}

/// Claims one runnable job and posts it. `Ok(None)` when nothing is waiting. Never panics on a
/// bad job: it ends up `failed` with a reason, or `retryable` when the database was unreachable.
pub fn run_next<S: Store + Send + Sync + 'static>(
    st: &AppState<S>,
    worker: &str,
    lease_secs: i64,
    max_attempts: i32,
) -> Result<Option<JobRun>, String> {
    let jobs = st.jobs.clone().ok_or("jobs are not turned on")?;
    let Some(job) = jobs
        .claim(worker, lease_secs, max_attempts)
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let done = |outcome: &'static str, detail: String| JobRun {
        job_id: job.id.clone(),
        tenant_id: job.tenant_id.clone(),
        outcome,
        detail,
    };
    let tenant = job.tenant_id.as_str();
    let end = |to: JobState, code: &str, msg: &str| {
        let _ = jobs.transition(tenant, &job.id, job.state, to, Some((code, msg)));
    };
    let Some(scope) = st.verifier.public_scope(tenant) else {
        end(
            JobState::Failed,
            "bad_tenant",
            "the job's tenant is not valid",
        );
        return Ok(Some(done("failed", "bad tenant".into())));
    };
    let (ops, checks) = match build(st, &*jobs, &job) {
        Ok(b) => b,
        Err(p) => {
            end(JobState::Failed, "bad_rows", &p.detail);
            return Ok(Some(done("failed", p.detail)));
        }
    };
    if job.state == JobState::Processing {
        if let Err(e) = jobs.transition(
            tenant,
            &job.id,
            JobState::Processing,
            JobState::Committing,
            None,
        ) {
            return Ok(Some(done(
                "failed",
                format!("could not start the commit: {e}"),
            )));
        }
    }
    // From here the job is `committing`; end() must name that state.
    let end = |to: JobState, code: &str, msg: &str| {
        let _ = jobs.transition(tenant, &job.id, JobState::Committing, to, Some((code, msg)));
    };
    let ctx = ce_validate::Context {
        actor_id: job.actor.clone(),
        actor_role: job.actor_role.clone(),
        now_unix: i64::try_from((st.now)()).unwrap_or(0),
    };
    let rid = format!("job:{}", job.id);
    let env = writes::Env {
        manifest: &st.manifest,
        scope: &scope,
        ctx: &ctx,
        request_id: &rid,
    };
    let _ = jobs.heartbeat(tenant, &job.id, worker, lease_secs, Some(0));
    match writes::execute_all_checked(&*st.store, &env, &ops, &checks, false) {
        Ok(records) => {
            let mut by_entity: Map<String, Value> = Map::new();
            for (e, _) in &ops {
                let n = by_entity.get(e).and_then(Value::as_i64).unwrap_or(0) + 1;
                by_entity.insert(e.clone(), json!(n));
            }
            let result = json!({"records": records.len(), "by_entity": by_entity});
            match jobs.complete(tenant, &job.id, &result) {
                Ok(_) => Ok(Some(done("completed", result.to_string()))),
                // The rows are committed; only the bookkeeping failed. The lease will expire and
                // the next pass replays every create to the same result and completes the job.
                Err(e) => Ok(Some(done(
                    "retry",
                    format!("posted but not marked done: {e}"),
                ))),
            }
        }
        Err(f) => {
            let (code, msg, retry) = classify(&f);
            if retry {
                end(JobState::Retryable, code, &msg);
                let _ =
                    jobs.transition(tenant, &job.id, JobState::Retryable, JobState::Queued, None);
                Ok(Some(done("retry", msg)))
            } else {
                end(JobState::Failed, code, &msg);
                Ok(Some(done("failed", msg)))
            }
        }
    }
}
