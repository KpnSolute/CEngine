//! CEngine HTTP server (draft v1): data endpoints over a [`Store`], with bearer-token auth,
//! role checks from the manifest, RFC 9457 errors and a generated OpenAPI document.
//!
//! Order of checks on every data request: token (401), installation (404), role (403),
//! entity exists (404). A caller without a grant gets 403 for known and unknown entities
//! alike, so entity names do not leak. Tenant comes only from the verified token.
//!
//! Endpoints: `GET|POST /v1/{inst}/{entity}` (POST needs an Idempotency-Key), `GET|PATCH|DELETE
//! /v1/{inst}/{entity}/{id}` (If-Match required on writes; PATCH is JSON Merge Patch),
//! `GET /_meta/health/{live,ready}`, `GET /_meta/manifest`, `GET /_meta/openapi`.
//! Field types and limits are checked by `ce-validate`. `GET /v1/{inst}/{entity}` lists by
//! keyset cursor on stores that support it (the typed Postgres store). Every write runs as one
//! store transaction with the parents whose totals it changes (see `writes`): computed fields
//! (including sums over child records) and invariants (including `ref()`) are enforced there.
//! Not yet: batch, actions, changesets, events, files.

mod actions;
mod changesets;
mod openapi;
mod periods;
mod problem;
mod writes;

pub use problem::Problem;

use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ce_auth::{Principal, Verifier};
use ce_manifest::Manifest;
use ce_policy::{Access, Policy};
use ce_store::{ListRequest, QueryError, Record, SortDir, SortKey, Store, StoreError};
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Arc};

const MAX_BODY: usize = 1024 * 1024;

pub struct AppState<S> {
    pub store: Arc<S>,
    pub verifier: Arc<Verifier>,
    pub policy: Arc<Policy>,
    pub manifest: Arc<Manifest>,
    pub installation: String,
    /// Unix seconds; injected so tests control time.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<S> Clone for AppState<S> {
    fn clone(&self) -> Self {
        AppState {
            store: self.store.clone(),
            verifier: self.verifier.clone(),
            policy: self.policy.clone(),
            manifest: self.manifest.clone(),
            installation: self.installation.clone(),
            now: self.now.clone(),
        }
    }
}

impl<S> AppState<S> {
    pub fn new(store: S, verifier: Verifier, manifest: Manifest, installation: &str) -> Self {
        AppState {
            store: Arc::new(store),
            verifier: Arc::new(verifier),
            policy: Arc::new(Policy::from_manifest(&manifest)),
            manifest: Arc::new(manifest),
            installation: installation.to_string(),
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        }
    }
}

pub fn router<S>(state: AppState<S>) -> Router
where
    S: Store + Send + Sync + 'static,
{
    Router::new()
        .route("/_meta/health/live", get(health))
        .route("/_meta/health/ready", get(health))
        .route("/_meta/manifest", get(manifest::<S>))
        .route("/_meta/openapi", get(openapi_doc::<S>))
        .route(
            "/v1/{inst}/changeset/{id}/{verb}",
            post(changeset_verb::<S>),
        )
        .route("/v1/{inst}/_batch", post(batch::<S>))
        .route("/v1/{inst}/actions/{name}", post(run_action::<S>))
        .route("/v1/{inst}/period/{id}/{verb}", post(period_verb::<S>))
        .route("/v1/{inst}/{entity}", get(list::<S>).post(create::<S>))
        .route(
            "/v1/{inst}/{entity}/{id}",
            get(read::<S>).patch(update::<S>).delete(remove::<S>),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

fn authenticate<S>(st: &AppState<S>, headers: &HeaderMap) -> Result<Principal, Problem> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(Problem::unauthenticated)?;
    st.verifier
        .verify_token(token, (st.now)())
        .map_err(|_| Problem::unauthenticated())
}

/// Token, installation, role, entity, in that order.
fn guard<S>(
    st: &AppState<S>,
    headers: &HeaderMap,
    inst: &str,
    entity: &str,
    access: Access,
) -> Result<Principal, Problem> {
    let who = authenticate(st, headers)?;
    if inst != st.installation {
        return Err(Problem::not_found());
    }
    st.policy
        .authorize(&who, access, entity)
        .map_err(|_| Problem::forbidden())?;
    if !st.manifest.value["entities"][entity].is_object() {
        return Err(Problem::not_found());
    }
    Ok(who)
}

fn bad_header(name: &str, why: &str) -> Problem {
    Problem::validation(
        format!("{name}: {why}"),
        vec![json!({"path": name, "code": "invalid_header", "detail": why})],
    )
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Problem> {
    match headers.get(name) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .map(Some)
            .map_err(|_| bad_header(name, "must be visible ASCII")),
    }
}

fn if_match(headers: &HeaderMap) -> Result<u64, Problem> {
    let raw = header_str(headers, "if-match")?
        .ok_or_else(|| Problem::precondition_required("If-Match"))?;
    raw.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .parse::<u64>()
        .map_err(|_| bad_header("If-Match", "must be a record version such as \"3\""))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, Problem> {
    let k = header_str(headers, "idempotency-key")?
        .ok_or_else(|| Problem::precondition_required("Idempotency-Key"))?;
    if k.is_empty() || k.len() > 255 {
        return Err(bad_header("Idempotency-Key", "must be 1 to 255 characters"));
    }
    Ok(k.to_string())
}

fn body_problem(e: BytesRejection) -> Problem {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        Problem::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("the body may be at most {MAX_BODY} bytes"),
        )
    } else {
        Problem::validation(
            "the body could not be read",
            vec![json!({"path": "", "code": "unreadable_body"})],
        )
    }
}

fn parse_object(body: &Bytes) -> Result<Map<String, Value>, Problem> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(Problem::validation(
            "the body must be a JSON object",
            vec![json!({"path": "", "code": "not_an_object"})],
        )),
        Err(_) => Err(Problem::validation(
            "the body is not valid JSON",
            vec![json!({"path": "", "code": "invalid_json"})],
        )),
    }
}

/// Field-level validation against the manifest, shared with every other host.
fn validate_body<S>(
    st: &AppState<S>,
    entity: &str,
    body: &Map<String, Value>,
    mode: ce_validate::Mode,
) -> Result<(), Problem> {
    ce_validate::validate(&st.manifest, entity, body, mode).map_err(|errors| {
        Problem::validation(
            "the body has invalid fields",
            errors
                .into_iter()
                .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail}))
                .collect(),
        )
    })
}

fn invariant_context<S>(st: &AppState<S>, who: &Principal) -> ce_validate::Context {
    ce_validate::Context {
        actor_id: who.user_id.clone(),
        actor_role: who.roles.first().cloned(),
        now_unix: i64::try_from((st.now)()).unwrap_or(0),
    }
}

pub(crate) fn failure_problem(f: writes::Failure) -> Problem {
    match f {
        writes::Failure::Store(e) => store_problem(e),
        writes::Failure::Rules(errors) => invariant_problem(errors),
        writes::Failure::Locked => locked_problem(),
    }
}

/// Sets what the server owns on a new record: the initial state and the declared defaults.
pub(crate) fn prepare_create(m: &Manifest, entity: &str, obj: &mut Map<String, Value>) {
    // Callers cannot choose the state; the server sets the initial one for every backend.
    let sm = &m.value["entities"][entity]["state"];
    if let (Some(f), Some(i)) = (sm["field"].as_str(), sm["initial"].as_str()) {
        obj.insert(f.to_string(), Value::String(i.to_string()));
    }
    // Defaults are written with the record, so later patches see them like any stored value.
    let fields = &m.value["entities"][entity]["fields"];
    for (name, f) in fields.as_object().into_iter().flatten() {
        if !obj.contains_key(name) && !f["computed"].is_string() && !f["default"].is_null() {
            obj.insert(name.clone(), f["default"].clone());
        }
    }
}

fn locked_problem() -> Problem {
    Problem::new(
        StatusCode::CONFLICT,
        "entity_locked",
        "the record is frozen; reopen its period to change it",
    )
}

fn invariant_problem(errors: Vec<ce_validate::FieldError>) -> Problem {
    Problem::validation(
        "the record breaks a rule of the template",
        errors
            .into_iter()
            .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail}))
            .collect(),
    )
}

/// RFC 7396 JSON Merge Patch.
pub(crate) fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(p) = patch {
        if !target.is_object() {
            *target = Value::Object(Map::new());
        }
        let t = target.as_object_mut().expect("just made an object");
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge_patch(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

fn store_problem(e: StoreError) -> Problem {
    match e {
        StoreError::NotFound => Problem::not_found(),
        StoreError::VersionConflict { current } => {
            let mut p = Problem::new(
                StatusCode::CONFLICT,
                "version_conflict",
                "the record changed since you read it",
            );
            p.retryable = true;
            p.extra.push(("current_version", json!(current)));
            p
        }
        StoreError::IdempotencyKeyReused => Problem::new(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "this Idempotency-Key was already used for a different request",
        ),
        StoreError::BadData(why) => Problem::validation(why, Vec::new()),
        StoreError::Constraint(kind) => Problem::new(
            StatusCode::CONFLICT,
            "constraint_violation",
            format!("the write breaks a {kind} rule"),
        ),
        // The cause goes to logs, never to the caller.
        StoreError::Query(q) => query_problem(q),
        StoreError::Unsupported => Problem::new(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "this installation's store cannot do that yet",
        ),
        StoreError::Backend(_) | StoreError::Aborted => Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "something went wrong on our side",
        ),
    }
}

async fn blocking<S, T>(
    st: &AppState<S>,
    f: impl FnOnce(&S) -> Result<T, StoreError> + Send + 'static,
) -> Result<T, Problem>
where
    S: Store + Send + Sync + 'static,
    T: Send + 'static,
{
    let store = st.store.clone();
    tokio::task::spawn_blocking(move || f(&store))
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
        .map_err(store_problem)
}

/// Refuses writes to what only the server writes: the audit trail, the changeset records and the
/// status fields of a period.
fn check_server_only(m: &Manifest, entity: &str, op: &writes::Op) -> Result<(), Problem> {
    // The audit trail is written by the server alone; nobody edits it through the API.
    if entity == writes::AUDIT_ENTITY || (changesets::enabled(m) && changesets::is_internal(entity))
    {
        return Err(Problem::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this record is written by the server only",
        ));
    }
    if entity == periods::PERIOD && periods::enabled(m) && periods::touches_server_fields(op) {
        return Err(Problem::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "a period is closed and reopened through its close and reopen endpoints",
        ));
    }
    Ok(())
}

/// Runs a create, patch or delete with everything it changes as one transaction.
async fn write<S>(
    st: &AppState<S>,
    scope: ce_auth::TenantScope,
    ctx: ce_validate::Context,
    entity: String,
    op: writes::Op,
    rid: &str,
) -> Result<Option<Record>, Problem>
where
    S: Store + Send + Sync + 'static,
{
    check_server_only(&st.manifest, &entity, &op)?;
    let rid = rid.to_string();
    let store = st.store.clone();
    let manifest = st.manifest.clone();
    tokio::task::spawn_blocking(move || {
        let env = writes::Env {
            manifest: &manifest,
            scope: &scope,
            ctx: &ctx,
            request_id: &rid,
        };
        writes::execute(&*store, &env, &entity, &op)
    })
    .await
    .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
    .map_err(|f| match f {
        writes::Failure::Store(e) => store_problem(e),
        writes::Failure::Rules(errors) => invariant_problem(errors),
        writes::Failure::Locked => locked_problem(),
    })
}

fn etag(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\"")).expect("digits and quotes are valid")
}

fn record_json(r: &Record) -> Value {
    let mut obj = r.data.as_object().cloned().unwrap_or_default();
    obj.insert("id".into(), json!(r.id));
    obj.insert("version".into(), json!(r.version));
    Value::Object(obj)
}

/// Callers without read access to the entity get only the id and version back, so a
/// write grant never discloses stored fields.
fn record_response(status: StatusCode, r: &Record, rid: &str, can_read: bool) -> Response {
    let data = if can_read {
        record_json(r)
    } else {
        json!({"id": r.id, "version": r.version})
    };
    let body = json!({"data": data, "meta": {"request_id": rid, "version": r.version}});
    let mut res = (status, Json(body)).into_response();
    res.headers_mut().insert(header::ETAG, etag(r.version));
    res
}

fn finish(rid: &str, r: Result<Response, Problem>) -> Response {
    r.unwrap_or_else(|p| p.with_request_id(rid).into_response())
}

async fn manifest<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    finish(
        &rid,
        authenticate(&st, &headers).map(|_| Json(st.manifest.value.clone()).into_response()),
    )
}

async fn openapi_doc<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    finish(
        &rid,
        authenticate(&st, &headers)
            .map(|_| Json(openapi::openapi(&st.manifest, &st.installation)).into_response()),
    )
}

/// A write waiting to be staged; owned so it can move into the blocking task.
struct Staged {
    operation: &'static str,
    record_id: Option<String>,
    expected: Option<u64>,
    data: Option<Value>,
    key: Option<String>,
}

impl Staged {
    fn new(
        operation: &'static str,
        record_id: Option<String>,
        expected: Option<u64>,
        data: Option<Value>,
        key: Option<String>,
    ) -> Self {
        Staged {
            operation,
            record_id,
            expected,
            data,
            key,
        }
    }
}

/// Stages the write as a changeset operation when the entity is governed and the caller may
/// not merge; the answer is then 202 with the changeset and operation ids.
async fn stage_if_governed<S: Store + Send + Sync + 'static>(
    st: &AppState<S>,
    who: &Principal,
    headers: &HeaderMap,
    entity: &str,
    w: Staged,
    rid: &str,
) -> Result<Option<Response>, Problem> {
    if !changesets::governed(&st.manifest, entity)
        || st.policy.can_run(&who.roles, changesets::MERGE_ACTION)
    {
        return Ok(None);
    }
    let append = header_str(headers, "x-ce-changeset")?.map(str::to_string);
    let (store, scope, actor, entity) = (
        st.store.clone(),
        who.scope().clone(),
        who.user_id.clone(),
        entity.to_string(),
    );
    let staged = tokio::task::spawn_blocking(move || {
        changesets::stage(
            &*store,
            &scope,
            &changesets::Staging {
                actor: &actor,
                entity: &entity,
                operation: w.operation,
                record_id: w.record_id.as_deref(),
                expected_version: w.expected,
                data: w.data,
                key: w.key.as_deref(),
                append_to: append.as_deref(),
            },
        )
    })
    .await
    .map_err(|_| store_problem(StoreError::Backend("task failed".into())))??;
    let body = json!({
        "data": {"changeset_id": staged.changeset_id, "op_id": staged.op_id, "status": "proposed"},
        "meta": {"request_id": rid}
    });
    Ok(Some((StatusCode::ACCEPTED, Json(body)).into_response()))
}

const MAX_BATCH: usize = 100;

/// `POST .../_batch` with `{"ops": [{"op": "create"|"update"|"delete", "entity": ..., ...}]}`:
/// every operation applies in one transaction, or none does.
async fn batch<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation {
            return Err(Problem::not_found());
        }
        let key = idempotency_key(&headers)?;
        let body = parse_object(&body.map_err(body_problem)?)?;
        let list = body.get("ops").and_then(Value::as_array).ok_or_else(|| {
            Problem::validation(
                "the body needs an ops list",
                vec![json!({"path": "/ops", "code": "required", "detail": "a list of operations"})],
            )
        })?;
        if list.is_empty() || list.len() > MAX_BATCH {
            return Err(Problem::validation(
                format!("a batch holds 1 to {MAX_BATCH} operations"),
                vec![
                    json!({"path": "/ops", "code": "out_of_range", "detail": "wrong number of operations"}),
                ],
            ));
        }
        let can_merge = st.policy.can_run(&who.roles, changesets::MERGE_ACTION);
        let mut ops: Vec<(String, writes::Op)> = Vec::new();
        let mut pending = Vec::new();
        for (i, item) in list.iter().enumerate() {
            let bad = |what: &str| {
                Problem::validation(
                    format!("operation {i}: {what}"),
                    vec![
                        json!({"path": format!("/ops/{i}"), "code": "invalid_operation", "detail": what}),
                    ],
                )
            };
            let entity = item["entity"]
                .as_str()
                .ok_or_else(|| bad("needs an entity"))?;
            if !st.manifest.value["entities"][entity].is_object() {
                return Err(bad("unknown entity"));
            }
            st.policy
                .authorize(&who, Access::Write, entity)
                .map_err(|_| Problem::forbidden())?;
            if changesets::governed(&st.manifest, entity) && !can_merge {
                return Err(bad(
                    "this entity is governed; propose the change as a changeset",
                ));
            }
            let id = item["id"].as_str().map(String::from);
            let version = item["version"].as_u64();
            let data = item["data"].as_object().cloned();
            let op = match item["op"].as_str() {
                Some("create") => {
                    let mut obj = data.ok_or_else(|| bad("create needs data"))?;
                    let check = obj.clone();
                    prepare_create(&st.manifest, entity, &mut obj);
                    pending.push((entity.to_string(), check, ce_validate::Mode::Create));
                    writes::Op::Create {
                        data: obj,
                        key: format!("batch:{key}:{i}"),
                    }
                }
                Some("update") => {
                    let patch = data.ok_or_else(|| bad("update needs data"))?;
                    pending.push((entity.to_string(), patch.clone(), ce_validate::Mode::Patch));
                    writes::Op::Patch {
                        id: id.ok_or_else(|| bad("update needs an id"))?,
                        expected: version.ok_or_else(|| bad("update needs the record version"))?,
                        patch,
                    }
                }
                Some("delete") => writes::Op::Delete {
                    id: id.ok_or_else(|| bad("delete needs an id"))?,
                    expected: version.ok_or_else(|| bad("delete needs the record version"))?,
                },
                _ => return Err(bad("op must be create, update or delete")),
            };
            check_server_only(&st.manifest, entity, &op)?;
            for (e, body, mode) in pending.drain(..) {
                validate_body(&st, &e, &body, mode)?;
            }
            ops.push((entity.to_string(), op));
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        let records = tokio::task::spawn_blocking(move || {
            let env = writes::Env {
                manifest: &manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            writes::execute_all(&*store, &env, &ops)
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
        .map_err(failure_problem)?;
        let data: Vec<Value> = records
            .iter()
            .map(|r| r.as_ref().map_or(Value::Null, record_json))
            .collect();
        let body = json!({"data": data, "meta": {"request_id": rid, "count": data.len()}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// `POST .../actions/{name}` with `{"id": ..., "input": {...}}`, `If-Match` and `Idempotency-Key`.
async fn run_action<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation || !actions::exists(&st.manifest, &name) {
            return Err(Problem::not_found());
        }
        if !st.policy.can_run(&who.roles, &name) {
            return Err(Problem::forbidden());
        }
        let expected = if_match(&headers)?;
        let key = idempotency_key(&headers)?;
        let mut body = parse_object(&body.map_err(body_problem)?)?;
        let id = match body.remove("id") {
            Some(Value::String(s)) => s,
            _ => {
                return Err(Problem::validation(
                    "the body needs the id of the record",
                    vec![json!({"path": "/id", "code": "required", "detail": "a record id"})],
                ))
            }
        };
        let input = match body.remove("input") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(o)) => o,
            Some(_) => {
                return Err(Problem::validation(
                    "input must be an object",
                    vec![json!({"path": "/input", "code": "type_mismatch", "detail": "an object"})],
                ))
            }
        };
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        let done = tokio::task::spawn_blocking(move || {
            let env = writes::Env {
                manifest: &manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            actions::run(
                &*store,
                &env,
                &actions::Run {
                    name: &name,
                    id: &id,
                    expected,
                    input: &input,
                    key: &key,
                },
            )
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))??;
        let body = json!({"data": record_json(&done.record), "meta": {
            "request_id": rid, "version": done.record.version,
            "created": done.created, "emitted": done.emitted}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// `POST .../period/{id}/{close|reopen}`.
async fn period_verb<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id, verb)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation
            || !periods::enabled(&st.manifest)
            || !matches!(verb.as_str(), "close" | "reopen")
        {
            return Err(Problem::not_found());
        }
        if !st.policy.can_run(&who.roles, periods::ACTION) {
            return Err(Problem::forbidden());
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        let rec = tokio::task::spawn_blocking(move || {
            let env = writes::Env {
                manifest: &manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            periods::transition(&*store, &env, &id, verb == "close")
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))??;
        let body =
            json!({"data": record_json(&rec), "meta": {"request_id": rid, "version": rec.version}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// `POST .../changeset/{id}/{merge|reject|revert|withdraw}`.
async fn changeset_verb<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id, verb)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation
            || !changesets::enabled(&st.manifest)
            || !matches!(verb.as_str(), "merge" | "reject" | "revert" | "withdraw")
        {
            return Err(Problem::not_found());
        }
        if verb != "withdraw" && !st.policy.can_run(&who.roles, changesets::MERGE_ACTION) {
            return Err(Problem::forbidden());
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        let rec = tokio::task::spawn_blocking(move || {
            let env = writes::Env {
                manifest: &manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            match verb.as_str() {
                "merge" => changesets::merge(&*store, &env, &id),
                "revert" => changesets::revert(&*store, &env, &id),
                "reject" => {
                    changesets::close(&*store, &scope, &ctx.actor_id, &id, "rejected", false)
                }
                _ => changesets::close(&*store, &scope, &ctx.actor_id, &id, "withdrawn", true),
            }
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))??;
        let body =
            json!({"data": record_json(&rec), "meta": {"request_id": rid, "version": rec.version}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

async fn create<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Write)?;
        let key = idempotency_key(&headers)?;
        let mut obj = parse_object(&body.map_err(body_problem)?)?;
        validate_body(&st, &entity, &obj, ce_validate::Mode::Create)?;
        prepare_create(&st.manifest, &entity, &mut obj);
        if let Some(res) = stage_if_governed(
            &st,
            &who,
            &headers,
            &entity,
            Staged::new(
                "create",
                None,
                None,
                Some(Value::Object(obj.clone())),
                Some(key.clone()),
            ),
            &rid,
        )
        .await?
        {
            return Ok(res);
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let readable = st.policy.can_read(&who.roles, &entity);
        let rec = write(
            &st,
            scope,
            ctx,
            entity.clone(),
            writes::Op::Create { data: obj, key },
            &rid,
        )
        .await?
        .ok_or_else(|| store_problem(StoreError::Backend("no record".into())))?;
        let mut res = record_response(StatusCode::CREATED, &rec, &rid, readable);
        if let Ok(loc) = HeaderValue::from_str(&format!("/v1/{inst}/{entity}/{}", rec.id)) {
            res.headers_mut().insert(header::LOCATION, loc);
        }
        Ok(res)
    };
    finish(&rid, run.await)
}

async fn read<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Read)?;
        let scope = who.scope().clone();
        let rec = blocking(&st, move |s| s.get(&scope, &entity, &id)).await?;
        let matches = header_str(&headers, "if-none-match")?.is_some_and(|v| {
            v.trim().trim_start_matches("W/").trim_matches('"') == rec.version.to_string()
        });
        if matches {
            let mut res = StatusCode::NOT_MODIFIED.into_response();
            res.headers_mut().insert(header::ETAG, etag(rec.version));
            return Ok(res);
        }
        Ok(record_response(StatusCode::OK, &rec, &rid, true))
    };
    finish(&rid, run.await)
}

async fn update<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Write)?;
        let expected = if_match(&headers)?;
        let patch = parse_object(&body.map_err(body_problem)?)?;
        validate_body(&st, &entity, &patch, ce_validate::Mode::Patch)?;
        if let Some(res) = stage_if_governed(
            &st,
            &who,
            &headers,
            &entity,
            Staged::new(
                "update",
                Some(id.clone()),
                Some(expected),
                Some(Value::Object(patch.clone())),
                None,
            ),
            &rid,
        )
        .await?
        {
            return Ok(res);
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let readable = st.policy.can_read(&who.roles, &entity);
        let rec = write(
            &st,
            scope,
            ctx,
            entity,
            writes::Op::Patch {
                id,
                expected,
                patch,
            },
            &rid,
        )
        .await?
        .ok_or_else(|| store_problem(StoreError::Backend("no record".into())))?;
        Ok(record_response(StatusCode::OK, &rec, &rid, readable))
    };
    finish(&rid, run.await)
}

async fn remove<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Write)?;
        let expected = if_match(&headers)?;
        if let Some(res) = stage_if_governed(
            &st,
            &who,
            &headers,
            &entity,
            Staged::new("delete", Some(id.clone()), Some(expected), None, None),
            &rid,
        )
        .await?
        {
            return Ok(res);
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        write(
            &st,
            scope,
            ctx,
            entity,
            writes::Op::Delete { id, expected },
            &rid,
        )
        .await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    };
    finish(&rid, run.await)
}

/// Maps a planner error to a 422 that names the offending request parameter.
fn query_problem(e: QueryError) -> Problem {
    let (path, code, detail) = match e {
        QueryError::UnknownEntity(_) => return Problem::not_found(),
        QueryError::UnknownField(f) => ("select", "unknown_field", format!("unknown field {f}")),
        QueryError::NotFilterable(f) => {
            ("filter", "not_filterable", format!("cannot filter on {f}"))
        }
        QueryError::BadOp(f, op) => (
            "filter",
            "bad_operator",
            format!("{op} is not allowed on {f}"),
        ),
        QueryError::BadValue(f) => (
            "filter",
            "bad_value",
            format!("the value for {f} has the wrong type"),
        ),
        QueryError::BadFilter(why) => ("filter", "bad_filter", why.to_string()),
        QueryError::FilterTooLarge => (
            "filter",
            "filter_too_large",
            "the filter is too large".to_string(),
        ),
        QueryError::UnindexedSort(f) => {
            ("sort", "unindexed_sort", format!("{f} is not a sort key"))
        }
        QueryError::BadCursor => (
            "after",
            "bad_cursor",
            "the cursor does not match this request".to_string(),
        ),
        QueryError::NotSearchable(e) => (
            "q",
            "not_searchable",
            format!("{e} has no searchable fields"),
        ),
        QueryError::BadSearch => (
            "q",
            "bad_search",
            "the search text must be 1 to 100 characters".to_string(),
        ),
        QueryError::BadSchema => {
            return store_problem(StoreError::Backend("bad schema".into()));
        }
    };
    Problem::validation(
        "the list request is invalid",
        vec![json!({"path": path, "code": code, "detail": detail})],
    )
}

fn bad_param(path: &str, detail: &str) -> Problem {
    Problem::validation(
        "the list request is invalid",
        vec![json!({"path": path, "code": "invalid_value", "detail": detail})],
    )
}

/// Builds the planner request from `?select=a,b&sort=-price,name&limit=50&after=..&filter=..&q=text`.
fn list_request(q: &HashMap<String, String>) -> Result<ListRequest, Problem> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    if q.values().any(|v| v.len() > 8192) {
        return Err(bad_param("query", "a parameter is too long"));
    }
    let names = |v: &str| v.split(',').map(str::to_string).collect::<Vec<_>>();
    let sort = q
        .get("sort")
        .map(|v| {
            names(v)
                .into_iter()
                .map(|f| match f.strip_prefix('-') {
                    Some(f) => SortKey {
                        field: f.to_string(),
                        dir: SortDir::Desc,
                    },
                    None => SortKey {
                        field: f,
                        dir: SortDir::Asc,
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    let limit = match q.get("limit") {
        Some(v) => Some(
            v.parse::<usize>()
                .map_err(|_| bad_param("limit", "limit must be a whole number"))?,
        ),
        None => None,
    };
    let filter = match q.get("filter") {
        Some(v) => Some(
            serde_json::from_str::<Value>(v)
                .map_err(|_| bad_param("filter", "filter must be JSON"))?,
        ),
        None => None,
    };
    let after = match q.get("after") {
        Some(v) => {
            let raw = B64
                .decode(v)
                .map_err(|_| bad_param("after", "the cursor is not valid"))?;
            match serde_json::from_slice::<Value>(&raw) {
                Ok(Value::Array(a)) => Some(a),
                _ => return Err(bad_param("after", "the cursor is not valid")),
            }
        }
        None => None,
    };
    Ok(ListRequest {
        select: q.get("select").map(|v| names(v)),
        filter,
        search: q.get("q").cloned(),
        sort,
        limit,
        after,
    })
}

async fn list<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Read)?;
        let req = list_request(&q)?;
        let scope = who.scope().clone();
        let page = blocking(&st, move |s| s.list(&scope, &entity, &req)).await?;
        let data: Vec<Value> = page.records.iter().map(record_json).collect();
        let next = page.next.map(|c| B64.encode(Value::Array(c).to_string()));
        Ok((
            StatusCode::OK,
            Json(json!({"data": data, "meta": {"request_id": rid, "next": next}})),
        )
            .into_response())
    };
    finish(&rid, run.await)
}
