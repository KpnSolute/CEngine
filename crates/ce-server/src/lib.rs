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
mod ai;
mod changesets;
mod events;
mod files;
mod login;
mod openapi;
mod periods;
mod pin_login;
mod problem;
mod reports;
mod slots;
mod staff;
mod staff_routes;
mod writes;

pub use files::{BlobStore, MemoryBlobs};

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

pub use ce_provider::{Provider, ProviderError, ProviderRequest, ProviderResponse};
pub use events::{relay_once, Deliver, Delivery, Relay, Report as RelayReport};
pub use login::{Identity, Issuing, Login, LoginError};
pub use pin_login::{
    hash_pin, hash_pin_random, parse_staff_file, weak_pin, DirectoryError, PinLogin, PinPolicy,
};
pub use staff::{
    AdminError, DirectoryFailure, MemoryDirectory, StaffAdmin, StaffDirectory, StaffInfo,
    StaffRecord,
};

const MAX_BODY: usize = 1024 * 1024;

pub struct AppState<S> {
    pub store: Arc<S>,
    pub verifier: Arc<Verifier>,
    pub policy: Arc<Policy>,
    pub manifest: Arc<Manifest>,
    pub installation: String,
    /// Unix seconds; injected so tests control time.
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Where file bytes live; without one the file endpoints answer 503.
    pub blobs: Option<Arc<dyn BlobStore>>,
    pub staff_admin_role: String,
    /// Turns the workspace name in a public URL into a tenant id; without one the public
    /// endpoints answer 404. The host's directory decides, never the caller.
    pub public_tenants: Option<PublicTenants>,
    /// Signs and delivers events out; without one `POST .../events/relay` answers 503.
    pub relay: Option<Arc<Relay>>,
    /// Turns the host's login into tokens; without one `POST .../_auth/token` answers 503.
    pub issuing: Option<Arc<Issuing>>,
    /// Where AI calls go (Infajn); without one `POST .../_ai/entry` answers 503.
    pub provider: Option<Arc<dyn ce_provider::Provider>>,
}

/// Looks up the tenant behind a public workspace name.
pub type PublicTenants = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

impl<S> Clone for AppState<S> {
    fn clone(&self) -> Self {
        AppState {
            store: self.store.clone(),
            verifier: self.verifier.clone(),
            policy: self.policy.clone(),
            manifest: self.manifest.clone(),
            installation: self.installation.clone(),
            now: self.now.clone(),
            blobs: self.blobs.clone(),
            staff_admin_role: self.staff_admin_role.clone(),
            public_tenants: self.public_tenants.clone(),
            relay: self.relay.clone(),
            issuing: self.issuing.clone(),
            provider: self.provider.clone(),
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
            blobs: None,
            staff_admin_role: "admin".to_string(),
            public_tenants: None,
            relay: None,
            issuing: None,
            provider: None,
        }
    }

    /// Attaches the host's provider handle (Infajn), which makes every call to an AI model.
    pub fn with_provider(mut self, provider: Arc<dyn ce_provider::Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Gives the server a signer and a transport for events out.
    pub fn with_relay(mut self, relay: Relay) -> Self {
        self.relay = Some(Arc::new(relay));
        self
    }

    /// Lets the host's login be exchanged for tokens scoped to `tenant`, valid `ttl_secs` (1 to
    /// 86,400). Fails if the tenant is not a lowercase uuid or the lifetime is out of range.
    pub fn with_login(
        mut self,
        login: Box<dyn Login>,
        tenant: &str,
        ttl_secs: u64,
    ) -> Result<Self, &'static str> {
        if self.verifier.public_scope(tenant).is_none() {
            return Err("the tenant must be a lowercase uuid");
        }
        if !(1..=86_400).contains(&ttl_secs) {
            return Err("the token lifetime is 1 to 86400 seconds");
        }
        self.issuing = Some(Arc::new(Issuing {
            login,
            tenant: tenant.to_string(),
            ttl_secs,
        }));
        Ok(self)
    }

    /// The role that may administer staff (`/_staff`); `admin` unless set.
    pub fn with_staff_admin_role(mut self, role: &str) -> Self {
        self.staff_admin_role = role.to_string();
        self
    }

    /// Lets public projections resolve workspace names to tenants.
    pub fn with_public_tenants(mut self, f: PublicTenants) -> Self {
        self.public_tenants = Some(f);
        self
    }

    /// Gives the server a place for file bytes.
    pub fn with_blobs(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.blobs = Some(blobs);
        self
    }
}

/// The largest file body: `runtime.storage.max_file_mb` (10 by default).
fn max_file_bytes(m: &Manifest) -> usize {
    let mb = m.value["runtime"]["storage"]["max_file_mb"]
        .as_u64()
        .unwrap_or(10);
    usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(10 * 1024 * 1024)
}

pub fn router<S>(state: AppState<S>) -> Router
where
    S: Store + Send + Sync + 'static,
{
    Router::new()
        .route("/_meta/health/live", get(health))
        .route("/_meta/health/ready", get(ready::<S>))
        .route("/_meta/manifest", get(manifest::<S>))
        .route("/_meta/openapi", get(openapi_doc::<S>))
        .route(
            "/v1/{inst}/changeset/{id}/{verb}",
            post(changeset_verb::<S>),
        )
        .route(
            "/v1/{inst}/_files/{entity}/{id}/{field}",
            get(file_get::<S>)
                .put(file_put::<S>)
                .layer(DefaultBodyLimit::max(max_file_bytes(&state.manifest))),
        )
        .route(
            "/public/{inst}/{workspace}/{projection}",
            get(public_read::<S>),
        )
        .route("/v1/{inst}/_batch", post(batch::<S>))
        .route("/v1/{inst}/_barcode", get(barcode::<S>))
        .route(
            "/v1/{inst}/_auth/token",
            post(issue_token::<S>).layer(DefaultBodyLimit::max(LOGIN_BYTES)),
        )
        .route("/v1/{inst}/_report/{name}", get(report::<S>))
        .route(
            "/v1/{inst}/_ai/entry",
            post(ai_entry::<S>).layer(DefaultBodyLimit::max(ai::MAX_BODY)),
        )
        .route(
            "/v1/{inst}/_auth/pin",
            post(staff_routes::change_pin::<S>).layer(DefaultBodyLimit::max(LOGIN_BYTES)),
        )
        .route(
            "/v1/{inst}/_staff",
            get(staff_routes::staff_list::<S>)
                .post(staff_routes::staff_create::<S>)
                .layer(DefaultBodyLimit::max(LOGIN_BYTES)),
        )
        .route(
            "/v1/{inst}/_staff/{username}",
            axum::routing::patch(staff_routes::staff_update::<S>)
                .layer(DefaultBodyLimit::max(LOGIN_BYTES)),
        )
        .route(
            "/v1/{inst}/_staff/{username}/reset_pin",
            post(staff_routes::staff_reset_pin::<S>).layer(DefaultBodyLimit::max(LOGIN_BYTES)),
        )
        .route("/v1/{inst}/_slots", get(free_slots::<S>))
        .route("/v1/{inst}/actions/{name}", post(run_action::<S>))
        .route("/v1/{inst}/period/{id}/{verb}", post(period_verb::<S>))
        .route("/v1/{inst}/{entity}/_export", get(export_csv::<S>))
        .route("/v1/{inst}/{entity}/_import", post(import_csv::<S>))
        .route("/v1/{inst}/events/relay", post(events_relay::<S>))
        .route("/v1/{inst}/events/{id}/replay", post(events_replay::<S>))
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

/// Ready only when the database answers; a load balancer stops sending traffic otherwise.
async fn ready<S>(State(st): State<AppState<S>>) -> Response
where
    S: Store + Send + Sync + 'static,
{
    match blocking(&st, |s| s.ping()).await {
        Ok(()) => Json(json!({"status": "ok"})).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "unavailable"})),
        )
            .into_response(),
    }
}

/// A verified token that may use the API. A token issued for a forced PIN change carries only
/// the role `_rotate_pin` and is refused everywhere except `POST .../_auth/pin`.
fn authenticate<S>(st: &AppState<S>, headers: &HeaderMap) -> Result<Principal, Problem> {
    let who = authenticate_any(st, headers)?;
    if who.has_role(login::ROTATE_ROLE) {
        return Err(Problem::forbidden());
    }
    Ok(who)
}

/// Like [`authenticate`], but also accepts the forced-PIN-change token.
fn authenticate_any<S>(st: &AppState<S>, headers: &HeaderMap) -> Result<Principal, Problem> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(Problem::unauthenticated)?;
    let who = st
        .verifier
        .verify_token(token, (st.now)())
        .map_err(|_| Problem::unauthenticated())?;
    if let Some(nb) = st
        .issuing
        .as_ref()
        .and_then(|i| i.login.not_before(&who.user_id))
    {
        if who.issued_at <= nb {
            return Err(Problem::unauthenticated());
        }
    }
    Ok(who)
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

/// The id a caller chose for a record it creates: a lowercase hyphenated UUID, never the nil
/// one. A valid one is taken out of the body before field validation (`id` is not a field) and
/// put back afterwards so the store uses it. It lets one batch or changeset create a record and
/// others that point at it. Ids are per tenant, so choosing one cannot reach another tenant's
/// record; one already in use fails with 409. Any other `id` is left in the body, where
/// validation reports it as `reserved_field`.
pub(crate) fn take_client_id(obj: &mut Map<String, Value>) -> Option<Value> {
    let ok = obj
        .get("id")
        .and_then(Value::as_str)
        .and_then(|s| uuid::Uuid::parse_str(s).ok().map(|u| (u, s)))
        .is_some_and(|(u, s)| !u.is_nil() && u.hyphenated().to_string() == s);
    if ok {
        obj.remove("id")
    } else {
        None
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
    if entity == writes::AUDIT_ENTITY
        || (changesets::enabled(m) && changesets::is_internal(entity))
        || (events::enabled(m) && entity == events::ENTITY)
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
/// The record as the caller may see it: everything with read access, else only id and version.
fn view(r: &Record, can_read: bool) -> Value {
    if can_read {
        record_json(r)
    } else {
        json!({"id": r.id, "version": r.version})
    }
}

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

/// The `file` field `field` of `entity`, or not found.
fn file_field<'a, S>(st: &'a AppState<S>, entity: &str, field: &str) -> Result<&'a Value, Problem> {
    let f = &st.manifest.value["entities"][entity]["fields"][field];
    if f["type"] == "file" {
        Ok(f)
    } else {
        Err(Problem::not_found())
    }
}

fn blobs_of<S>(st: &AppState<S>) -> Result<Arc<dyn BlobStore>, Problem> {
    st.blobs.clone().ok_or_else(|| {
        Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "files_not_configured",
            "this installation has no file storage",
        )
    })
}

/// `PUT .../_files/{entity}/{id}/{field}`: the body is the file. Needs `If-Match`.
async fn file_put<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity, id, field)): Path<(String, String, String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Write)?;
        file_field(&st, &entity, &field)?;
        let blobs = blobs_of(&st)?;
        let expected = if_match(&headers)?;
        let bytes = body.map_err(body_problem)?;
        if bytes.is_empty() {
            return Err(Problem::validation(
                "the body is empty",
                vec![
                    json!({"path": "", "code": "required", "detail": "send the file as the body"}),
                ],
            ));
        }
        if changesets::governed(&st.manifest, &entity)
            && !st.policy.can_run(&who.roles, changesets::MERGE_ACTION)
        {
            return Err(Problem::validation(
                "this entity is governed; files cannot be attached through a proposal yet",
                vec![json!({"path": "", "code": "governed", "detail": "governed entity"})],
            ));
        }
        let content_type = header_str(&headers, "content-type")?
            .unwrap_or("application/octet-stream")
            .to_string();
        let name = files::clean_name(header_str(&headers, "x-ce-filename")?.unwrap_or("file"));
        let sha = files::sha256_hex(&bytes);
        let scope = who.scope().clone();
        let key = files::object_key(&scope, &entity, &id, &field, &sha);
        let meta = json!({"key": key, "sha256": sha, "size": bytes.len(),
                          "content_type": content_type, "name": name});
        let mut patch = Map::new();
        patch.insert(field.clone(), meta);
        let readable = st.policy.can_read(&who.roles, &entity);
        let (put_scope, put_key, put_type) = (scope.clone(), key.clone(), content_type.clone());
        tokio::task::spawn_blocking(move || blobs.put(&put_scope, &put_key, &bytes, &put_type))
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
            .map_err(|e| store_problem(StoreError::Backend(e)))?;
        let ctx = invariant_context(&st, &who);
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

/// `GET .../_files/{entity}/{id}/{field}`: the file's bytes.
async fn file_get<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity, id, field)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Read)?;
        file_field(&st, &entity, &field)?;
        let blobs = blobs_of(&st)?;
        let scope = who.scope().clone();
        let (s2, e2, rec_id) = (scope.clone(), entity.clone(), id.clone());
        let rec = blocking(&st, move |s| s.get(&s2, &e2, &id)).await?;
        let meta = rec
            .data
            .get(&field)
            .filter(|m| m.is_object())
            .ok_or_else(Problem::not_found)?;
        let key = meta["key"]
            .as_str()
            .ok_or_else(Problem::not_found)?
            .to_string();
        // The key must belong to this very record and field, whatever the stored value says:
        // a file field written through a plain patch cannot point at someone else's file.
        if !key.starts_with(&format!("{}/{entity}/{rec_id}/{field}/", scope.tenant_id())) {
            return Err(Problem::not_found());
        }
        // The hash is the last part of the key, which the prefix check above ties to this record;
        // the stored `sha256` value is not trusted. The type is the committed one, not whatever
        // an upload that was later rejected left in the store.
        let sha = key.rsplit('/').next().unwrap_or_default().to_string();
        let stored_type = meta["content_type"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_string();
        let name = files::clean_name(meta["name"].as_str().unwrap_or("file"));
        let got = tokio::task::spawn_blocking(move || blobs.get(&scope, &key))
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
            .map_err(|e| store_problem(StoreError::Backend(e)))?;
        let (bytes, _) = got.ok_or_else(Problem::not_found)?;
        let content_type = stored_type;
        let mut res = (StatusCode::OK, bytes).into_response();
        let h = res.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&content_type) {
            h.insert(header::CONTENT_TYPE, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("\"{sha}\"")) {
            h.insert(header::ETAG, v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        h.insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        Ok(res)
    };
    finish(&rid, run.await)
}

/// `GET /public/{inst}/{workspace}/{projection}`: no login. Only the fields and rows the
/// manifest's projection allows, never an id, a version or anything else of the record.
async fn public_read<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, workspace, name)): Path<(String, String, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    let rid = request_id();
    let run = async {
        let proj = &st.manifest.value["projections"][name.as_str()];
        if inst != st.installation
            || st.manifest.value["runtime"]["network"]["public_projections"] != true
            || !proj.is_object()
        {
            return Err(Problem::not_found());
        }
        let tenant = st
            .public_tenants
            .as_ref()
            .and_then(|f| f(&workspace))
            .ok_or_else(Problem::not_found)?;
        let scope = st
            .verifier
            .public_scope(&tenant)
            .ok_or_else(Problem::not_found)?;
        let entity = proj["entity"]
            .as_str()
            .ok_or_else(Problem::not_found)?
            .to_string();
        let fields: Vec<String> = proj["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(String::from))
            .collect();
        let filter = match proj["filter"].as_str() {
            Some(src) => Some(ce_manifest::filter_json(src).map_err(|e| {
                store_problem(StoreError::Backend(format!("projection filter: {e}")))
            })?),
            None => None,
        };
        let mut req = list_request(&HashMap::new())?;
        // Callers page; they cannot change what is selected, filtered, sorted or searched.
        req.select = Some(fields.clone());
        req.filter = filter;
        req.limit = Some(
            q.get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50)
                .clamp(1, 100),
        );
        if let Some(c) = q.get("after") {
            let raw = B64
                .decode(c)
                .map_err(|_| bad_param("after", "the cursor is not valid"))?;
            match serde_json::from_slice::<Value>(&raw) {
                Ok(Value::Array(a)) => req.after = Some(a),
                _ => return Err(bad_param("after", "the cursor is not valid")),
            }
        }
        let store = st.store.clone();
        let page = tokio::task::spawn_blocking(move || store.list(&scope, &entity, &req))
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
            .map_err(store_problem)?;
        let data: Vec<Value> = page
            .records
            .iter()
            .map(|r| {
                let mut o = Map::new();
                for f in &fields {
                    if let Some(v) = r.data.get(f) {
                        o.insert(f.clone(), v.clone());
                    }
                }
                Value::Object(o)
            })
            .collect();
        let next = page.next.map(|c| B64.encode(Value::Array(c).to_string()));
        let mut res = (
            StatusCode::OK,
            Json(json!({"data": data, "meta": {"request_id": rid, "next": next}})),
        )
            .into_response();
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=30"),
        );
        Ok(res)
    };
    finish(&rid, run.await)
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
                    let chosen = take_client_id(&mut obj);
                    let check = obj.clone();
                    prepare_create(&st.manifest, entity, &mut obj);
                    if let Some(id) = chosen {
                        obj.insert("id".into(), id);
                    }
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
        let entities: Vec<String> = ops.iter().map(|(e, _)| e.clone()).collect();
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
            .zip(&entities)
            .map(|(r, e)| {
                let readable = st.policy.can_read(&who.roles, e);
                r.as_ref().map_or(Value::Null, |r| view(r, readable))
            })
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
        let readable = st.manifest.value["actions"][name.as_str()]["entity"]
            .as_str()
            .is_some_and(|e| st.policy.can_read(&who.roles, e));
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
        let body = json!({"data": view(&done.record, readable), "meta": {
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
        let body = json!({"data": view(&rec, st.policy.can_read(&who.roles, periods::PERIOD)), "meta": {"request_id": rid, "version": rec.version}});
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
            || !matches!(
                verb.as_str(),
                "merge" | "reject" | "revert" | "withdraw" | "preview"
            )
        {
            return Err(Problem::not_found());
        }
        if verb != "withdraw" && !st.policy.can_run(&who.roles, changesets::MERGE_ACTION) {
            return Err(Problem::forbidden());
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        if verb == "preview" {
            let data = tokio::task::spawn_blocking(move || {
                let env = writes::Env {
                    manifest: &manifest,
                    scope: &scope,
                    ctx: &ctx,
                    request_id: &rid2,
                };
                changesets::preview(&*store, &env, &id)
            })
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))??;
            let body = json!({"data": data, "meta": {"request_id": rid}});
            return Ok((StatusCode::OK, Json(body)).into_response());
        }
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
        let body = json!({"data": view(&rec, st.policy.can_read(&who.roles, changesets::CHANGESET)), "meta": {"request_id": rid, "version": rec.version}});
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
        let chosen = take_client_id(&mut obj);
        validate_body(&st, &entity, &obj, ce_validate::Mode::Create)?;
        prepare_create(&st.manifest, &entity, &mut obj);
        if let Some(id) = chosen {
            obj.insert("id".into(), id);
        }
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

/// `POST .../events/{id}/replay`: puts a delivered or dead-lettered event back in the queue.
async fn events_replay<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation || !events::enabled(&st.manifest) {
            return Err(Problem::not_found());
        }
        if !st.policy.can_run(&who.roles, events::ACTION) {
            return Err(Problem::forbidden());
        }
        let scope = who.scope().clone();
        let store = st.store.clone();
        let rec = tokio::task::spawn_blocking(move || events::replay(&*store, &scope, &id))
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
            .map_err(|e| match e {
                StoreError::Constraint("not_replayable") => Problem::new(
                    StatusCode::CONFLICT,
                    "invalid_transition",
                    "only a delivered or dead-lettered event can be replayed",
                ),
                e => store_problem(e),
            })?;
        let body = json!({"data": view(&rec, st.policy.can_read(&who.roles, events::ENTITY)), "meta": {"request_id": rid, "version": rec.version}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// `POST .../events/relay`: one delivery pass for the caller's tenant.
async fn events_relay<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation || !events::enabled(&st.manifest) {
            return Err(Problem::not_found());
        }
        if !st.policy.can_run(&who.roles, events::ACTION) {
            return Err(Problem::forbidden());
        }
        let Some(relay) = st.relay.clone() else {
            return Err(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "no event transport is configured",
            ));
        };
        let scope = who.scope().clone();
        let (store, now) = (st.store.clone(), (st.now)());
        let r =
            tokio::task::spawn_blocking(move || events::relay_once(&*store, &scope, &relay, now))
                .await
                .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
                .map_err(store_problem)?;
        let body = json!({"data": {"delivered": r.delivered, "retrying": r.retrying, "dead_lettered": r.dead_lettered}, "meta": {"request_id": rid}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// The most rows one export returns; a bigger result must be narrowed with `filter`.
const EXPORT_ROWS: usize = ce_io::MAX_ROWS;
/// The most rows one import creates (all in one transaction).
const IMPORT_ROWS: usize = 1000;

/// `GET .../{entity}/_export?filter=..&sort=..`: the entity's records as CSV (see `ce-io`).
async fn export_csv<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Read)?;
        let mut req = list_request(&q)?;
        req.select = None;
        req.limit = Some(200);
        req.after = None;
        let scope = who.scope().clone();
        let ent = entity.clone();
        let records = blocking(&st, move |s| {
            let mut all: Vec<Value> = Vec::new();
            loop {
                let page = s.list(&scope, &ent, &req)?;
                all.extend(page.records.iter().map(record_json));
                if all.len() > EXPORT_ROWS {
                    return Ok(None);
                }
                match page.next {
                    Some(c) => req.after = Some(c),
                    None => return Ok(Some(all)),
                }
            }
        })
        .await?
        .ok_or_else(|| {
            Problem::validation(
                format!("more than {EXPORT_ROWS} rows; narrow the export with filter"),
                Vec::new(),
            )
        })?;
        let csv =
            ce_io::export_csv(&st.manifest, &entity, &records).map_err(|_| Problem::not_found())?;
        let mut res = (StatusCode::OK, csv).into_response();
        let h = res.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/csv; charset=utf-8"),
        );
        if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{entity}.csv\"")) {
            h.insert(header::CONTENT_DISPOSITION, v);
        }
        Ok(res)
    };
    finish(&rid, run.await)
}

/// `POST .../{entity}/_import[?dry_run=true]` with a CSV body: every row is checked, and if all
/// pass they are created in one transaction (all or none). Any bad row answers 422 with the
/// report and creates nothing; `dry_run` answers the report and creates nothing.
async fn import_csv<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, entity)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = guard(&st, &headers, &inst, &entity, Access::Write)?;
        let key = idempotency_key(&headers)?;
        let dry = q.get("dry_run").is_some_and(|v| v == "true");
        if changesets::governed(&st.manifest, &entity)
            && !st.policy.can_run(&who.roles, changesets::MERGE_ACTION)
        {
            return Err(Problem::validation(
                "this entity is governed; propose the rows as a changeset",
                Vec::new(),
            ));
        }
        let bytes = body.map_err(body_problem)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Problem::validation("the file must be UTF-8", Vec::new()))?;
        let file_problems = |errs: Vec<ce_io::IoError>| {
            Problem::validation(
                "the file cannot be imported",
                errs.into_iter()
                    .map(|e| json!({"path": format!("/{}", e.column), "code": e.code, "detail": e.detail}))
                    .collect(),
            )
        };
        let report = ce_io::import_csv(&st.manifest, &entity, text).map_err(file_problems)?;
        if report.rows.len() > IMPORT_ROWS {
            return Err(Problem::validation(
                format!("an import holds at most {IMPORT_ROWS} rows"),
                Vec::new(),
            ));
        }
        let rows: Vec<Value> = report
            .rows
            .iter()
            .map(|r| {
                json!({"row": r.row, "ok": r.errors.is_empty(), "errors": r.errors.iter()
                    .map(|e| json!({"column": e.column, "code": e.code, "detail": e.detail}))
                    .collect::<Vec<_>>()})
            })
            .collect();
        if report.rejected().next().is_some() {
            let mut p =
                Problem::validation("some rows are invalid; nothing was imported", Vec::new());
            p.extra.push(("rows", Value::Array(rows)));
            return Err(p);
        }
        // Row keys carry a digest of the whole file, so a retry of the same file replays every
        // row and a different file under the same key never mixes with it.
        let digest = {
            use sha2::{Digest, Sha256};
            let d = Sha256::digest(bytes.as_ref());
            d.iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let mut ops: Vec<(String, writes::Op)> = Vec::new();
        for r in &report.rows {
            let mut obj = r.body.clone();
            prepare_create(&st.manifest, &entity, &mut obj);
            let op = writes::Op::Create {
                data: obj,
                key: format!("import:{key}:{digest}:{}", r.row),
            };
            check_server_only(&st.manifest, &entity, &op)?;
            ops.push((entity.clone(), op));
        }
        let ctx = invariant_context(&st, &who);
        let scope = who.scope().clone();
        let (store, manifest, rid2) = (st.store.clone(), st.manifest.clone(), rid.clone());
        let made = tokio::task::spawn_blocking(move || {
            let env = writes::Env {
                manifest: &manifest,
                scope: &scope,
                ctx: &ctx,
                request_id: &rid2,
            };
            writes::execute_all_with(&*store, &env, &ops, dry)
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
        .map_err(failure_problem)?;
        if dry {
            let n = rows.len();
            return Ok((
                StatusCode::OK,
                Json(json!({"data": {"rows": rows, "created": 0}, "meta": {"request_id": rid, "dry_run": true, "count": n}})),
            )
                .into_response());
        }
        let ids: Vec<Value> = made.iter().flatten().map(|r| json!(r.id)).collect();
        let body = json!({"data": {"created": ids.len(), "ids": ids}, "meta": {"request_id": rid}});
        Ok((StatusCode::CREATED, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// `GET .../_barcode?type=code128|ean13|upca|qr&data=..[&module_size=..&bar_height=..&quiet_zone=..]`:
/// the symbol as SVG. Needs a valid token for the installation, nothing else: no tenant data is
/// read and nothing is stored. A bad value answers 422 with the position of the first bad character.
async fn barcode<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    use ce_barcode::{BarcodeError, Symbology};
    let rid = request_id();
    let run = async {
        authenticate(&st, &headers)?;
        if inst != st.installation {
            return Err(Problem::not_found());
        }
        let bad = |param: &str, why: &str| {
            Problem::validation(
                format!("{param}: {why}"),
                vec![json!({"path": format!("/{param}"), "code": "invalid", "detail": why})],
            )
        };
        let symbology = match q.get("type").map(String::as_str) {
            Some("code128") => Symbology::Code128,
            Some("ean13") => Symbology::Ean13,
            Some("upca") => Symbology::UpcA,
            Some("qr") => Symbology::Qr,
            _ => return Err(bad("type", "one of code128, ean13, upca, qr")),
        };
        let data = q
            .get("data")
            .ok_or_else(|| bad("data", "the text to encode"))?;
        let number = |name: &str| -> Result<Option<u32>, Problem> {
            q.get(name)
                .map(|v| v.parse::<u32>().map_err(|_| bad(name, "a whole number")))
                .transpose()
        };
        let mut opt = ce_barcode::SvgOptions::default();
        if let Some(v) = number("module_size")? {
            opt.module_size = v;
        }
        if let Some(v) = number("bar_height")? {
            opt.bar_height = v;
        }
        opt.quiet_zone = number("quiet_zone")?;
        let failed = |e: BarcodeError| {
            // `BadOptions` messages start with the option's name (`module_size must be ...`).
            let path = match &e {
                BarcodeError::BadOptions(why) => {
                    format!("/{}", why.split_whitespace().next().unwrap_or("data"))
                }
                _ => "/data".to_string(),
            };
            let (code, detail) = match e {
                BarcodeError::Empty => ("empty", "nothing to encode".to_string()),
                BarcodeError::TooLong { max } => ("too_long", format!("at most {max}")),
                BarcodeError::BadChar { index } => {
                    ("bad_char", format!("character {index} cannot be encoded"))
                }
                BarcodeError::BadLength { expected, got } => {
                    ("bad_length", format!("expected {expected}, got {got}"))
                }
                BarcodeError::BadCheckDigit { expected } => (
                    "bad_check_digit",
                    format!("the check digit should be {expected}"),
                ),
                BarcodeError::Qr(_) => ("qr", "the data does not fit a QR code".to_string()),
                BarcodeError::BadOptions(why) => ("invalid", why.to_string()),
            };
            Problem::validation(
                "the barcode cannot be made",
                vec![json!({"path": path, "code": code, "detail": detail})],
            )
        };
        let code = ce_barcode::encode(symbology, data).map_err(failed)?;
        let svg = ce_barcode::render_svg(&code, &opt).map_err(failed)?;
        let mut res = (StatusCode::OK, svg).into_response();
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/svg+xml"),
        );
        Ok(res)
    };
    finish(&rid, run.await)
}

/// `GET .../_report/{name}[?format=json|csv|html&<param>=<value>]`: a declared report. Needs read
/// access to the report's entity. At most 1,000 groups come back; beyond that the answer says so
/// (`meta.truncated`, or the `X-CE-Truncated` header for CSV and HTML).
async fn report<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        if inst != st.installation || !reports::exists(&st.manifest, &name) {
            return Err(Problem::not_found());
        }
        let ent = reports::entity(&st.manifest, &name);
        st.policy
            .authorize(&who, Access::Read, &ent)
            .map_err(|_| Problem::forbidden())?;
        let format = q.get("format").map_or("json", String::as_str);
        if !matches!(format, "json" | "csv" | "html") {
            return Err(Problem::validation(
                "format: one of json, csv, html",
                vec![
                    json!({"path": "/format", "code": "invalid", "detail": "one of json, csv, html"}),
                ],
            ));
        }
        let req = reports::request(&st.manifest, &name, &q)?;
        let scope = who.scope().clone();
        let mut rows = blocking(&st, move |s| s.aggregate(&scope, &ent, &req))
            .await
            .map_err(|p| {
                if p.status == StatusCode::NOT_IMPLEMENTED
                    || p.status == StatusCode::SERVICE_UNAVAILABLE
                {
                    reports::unsupported()
                } else {
                    p
                }
            })?;
        let truncated = rows.len() > ce_store::MAX_GROUPS;
        rows.truncate(ce_store::MAX_GROUPS);
        let cols = reports::columns(&st.manifest, &name);
        let title = reports::title(&st.manifest, &name);
        let cells = reports::rows_as_cells(&cols, &rows);
        let mut res = match format {
            "csv" => {
                let header: Vec<String> = cols.iter().map(|c| c.0.clone()).collect();
                let textual: Vec<bool> = cols.iter().map(|c| c.1).collect();
                let mut r =
                    (StatusCode::OK, ce_io::table_csv(&header, &textual, &cells)).into_response();
                r.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/csv; charset=utf-8"),
                );
                if let Ok(v) =
                    HeaderValue::from_str(&format!("attachment; filename=\"{name}.csv\""))
                {
                    r.headers_mut().insert(header::CONTENT_DISPOSITION, v);
                }
                r
            }
            "html" => {
                let generated = ce_events::rfc3339((st.now)());
                let mut r = (
                    StatusCode::OK,
                    reports::html(&title, &generated, &cols, &cells, truncated),
                )
                    .into_response();
                let h = r.headers_mut();
                h.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                );
                h.insert(
                    header::CONTENT_SECURITY_POLICY,
                    HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'"),
                );
                h.insert(
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                );
                r
            }
            _ => {
                let body = json!({
                    "data": {"title": title, "columns": cols.iter().map(|c| c.0.clone()).collect::<Vec<_>>(), "rows": rows},
                    "meta": {"request_id": rid, "count": rows.len(), "truncated": truncated}
                });
                (StatusCode::OK, Json(body)).into_response()
            }
        };
        if truncated && format != "json" {
            res.headers_mut()
                .insert("x-ce-truncated", HeaderValue::from_static("true"));
        }
        Ok(res)
    };
    finish(&rid, run.await)
}

/// The most bytes of credentials one login request may carry; enforced while the body is read,
/// because the route is open to callers with no token.
const LOGIN_BYTES: usize = 4096;

/// `POST .../_auth/token` with the host's credentials as a JSON object: answers a signed token
/// for the configured tenant. No bearer token is needed (this is how one is obtained). A wrong
/// credential is always the same 401, whatever part was wrong.
async fn issue_token<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        if inst != st.installation {
            return Err(Problem::not_found());
        }
        let Some(issuing) = st.issuing.clone() else {
            return Err(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "no login is configured",
            ));
        };
        let body = body.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                Problem::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "payload_too_large",
                    format!("the credentials may be at most {LOGIN_BYTES} bytes"),
                )
            } else {
                body_problem(e)
            }
        })?;
        let credentials = parse_object(&body)?;
        let checked = tokio::task::spawn_blocking({
            let issuing = issuing.clone();
            move || issuing.login.login(&credentials)
        })
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?;
        let who = match checked {
            Ok(who) => who,
            Err(LoginError::Denied) => {
                return Err(Problem::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid_credentials",
                    "those credentials were not accepted",
                ))
            }
            Err(LoginError::Throttled) => {
                let mut p = Problem::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "throttled",
                    "too many attempts, try again later",
                );
                p.retryable = true;
                return Err(p);
            }
            Err(LoginError::Unavailable) => {
                let mut p = Problem::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "sign-in is unavailable right now",
                );
                p.retryable = true;
                return Err(p);
            }
        };
        // A forced PIN change gets a token with one role no template defines, good only for
        // `POST .../_auth/pin`.
        let roles = if who.must_rotate {
            vec![login::ROTATE_ROLE.to_string()]
        } else {
            login::known_roles(&st.manifest, &who.roles)
        };
        if roles.is_empty() {
            // Signed in but holding nothing this template defines: same answer as a bad login.
            return Err(Problem::new(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "those credentials were not accepted",
            ));
        }
        let now = (st.now)();
        let token = st
            .verifier
            .sign_token(&who.sub, &issuing.tenant, &roles, now, issuing.ttl_secs)
            .map_err(|_| store_problem(StoreError::Backend("cannot sign".into())))?;
        let body = json!({"data": {"token": token, "expires_at": now + issuing.ttl_secs, "roles": roles, "sub": who.sub, "must_rotate": who.must_rotate},
            "meta": {"request_id": rid}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

/// The most bookings of one resource read to work out its free slots.
const SLOT_BOOKINGS: usize = 2000;

/// `GET .../_slots?resource=<id>&from=<time>&to=<time>&duration=<minutes>[&step=<minutes>]`: the
/// bookable slots of one resource inside its opening hours, avoiding its requested and confirmed
/// bookings (recurring ones expanded) plus the resource's buffer. Needs read access to `resource`
/// and `booking`. `step` defaults to `duration`. At most 1,000 slots, in time order.
async fn free_slots<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        if !slots::enabled(&st.manifest) {
            authenticate(&st, &headers)?;
            return Err(Problem::not_found());
        }
        let who = guard(&st, &headers, &inst, "booking", Access::Read)?;
        st.policy
            .authorize(&who, Access::Read, "resource")
            .map_err(|_| Problem::forbidden())?;
        let invalid = |(path, detail): slots::Bad| {
            Problem::validation(
                format!("{path}: {detail}"),
                vec![json!({"path": path, "code": "invalid", "detail": detail})],
            )
        };
        let need = |name: &str| {
            q.get(name)
                .ok_or_else(|| invalid((format!("/{name}"), "required".into())))
        };
        let minutes =
            |name: &str, default: Option<i64>| -> Result<ce_calendar::Duration, Problem> {
                let n = match (q.get(name), default) {
                    (Some(v), _) => v.parse::<i64>().map_err(|_| {
                        invalid((format!("/{name}"), "a whole number of minutes".into()))
                    })?,
                    (None, Some(d)) => d,
                    (None, None) => return Err(invalid((format!("/{name}"), "required".into()))),
                };
                if !(1..=1440).contains(&n) {
                    return Err(invalid((format!("/{name}"), "1 to 1440 minutes".into())));
                }
                Ok(ce_calendar::Duration::minutes(n))
            };
        let id = need("resource")?.clone();
        let from = slots::instant("/from", need("from")?).map_err(invalid)?;
        let to = slots::instant("/to", need("to")?).map_err(invalid)?;
        let window = ce_calendar::Interval::new(from, to)
            .map_err(|_| invalid(("/to".into(), "must be after from".into())))?;
        let duration = minutes("duration", None)?;
        let step = match q.get("step") {
            Some(_) => minutes("step", None)?,
            None => duration,
        };
        let scope = who.scope().clone();
        let (resource, bookings) = blocking(&st, move |s| {
            let resource = s.get(&scope, "resource", &id)?;
            let filter = json!({"and": [
                {"field": "resource", "op": "eq", "value": id},
                {"field": "status", "op": "in", "value": ["requested", "confirmed"]}]});
            let mut rows = Vec::new();
            let mut after = None;
            loop {
                let req = ListRequest {
                    filter: Some(filter.clone()),
                    limit: Some(200),
                    after: after.clone(),
                    ..ListRequest::default()
                };
                let page = s.list(&scope, "booking", &req)?;
                rows.extend(page.records.into_iter().map(|r| r.data));
                if rows.len() > SLOT_BOOKINGS {
                    return Err(StoreError::BadData(
                        "the resource has too many open bookings to compute slots",
                    ));
                }
                match page.next {
                    Some(n) => after = Some(n),
                    None => break,
                }
            }
            Ok((resource.data, rows))
        })
        .await?;
        let found = slots::free(&resource, &bookings, &window, duration, step).map_err(invalid)?;
        let data: Vec<Value> = found
            .iter()
            .map(|s| json!({"starts_at": s.start.to_rfc3339(), "ends_at": s.end.to_rfc3339()}))
            .collect();
        Ok((
            StatusCode::OK,
            Json(json!({"data": data, "meta": {"request_id": rid}})),
        )
            .into_response())
    };
    finish(&rid, run.await)
}

/// `POST .../_ai/entry` with `{"entity": "...", "text": "...", "hint": "..."}`: draft records for
/// `entity` extracted from `text` by the host's AI provider, each checked like a create and
/// answered as `{drafts: [{body, errors}], ready}`. Needs write access to the entity. Nothing is
/// saved: the caller reviews and saves through the ordinary write routes.
async fn ai_entry<S: Store + Send + Sync + 'static>(
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
        let body = parse_object(&body.map_err(body_problem)?)?;
        let bad = |path: &str, why: &str| {
            Problem::validation(
                format!("{path}: {why}"),
                vec![json!({"path": path, "code": "invalid", "detail": why})],
            )
        };
        let entity = body
            .get("entity")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("/entity", "the entity to draft records for"))?
            .to_string();
        let text = body
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| bad("/text", "the text to read"))?
            .to_string();
        if text.len() > ai::MAX_TEXT {
            return Err(bad("/text", "too long"));
        }
        let hint = match body.get("hint") {
            None | Some(Value::Null) => None,
            Some(Value::String(h)) if h.len() <= 500 => Some(h.clone()),
            _ => return Err(bad("/hint", "a string of at most 500 bytes")),
        };
        st.policy
            .authorize(&who, Access::Write, &entity)
            .map_err(|_| Problem::forbidden())?;
        if !st.manifest.value["entities"][&entity].is_object() {
            return Err(Problem::not_found());
        }
        check_server_only(
            &st.manifest,
            &entity,
            &writes::Op::Create {
                data: Map::new(),
                key: String::new(),
            },
        )?;
        let Some(provider) = st.provider.clone() else {
            return Err(Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "no AI provider is configured",
            ));
        };
        let scope = who.scope().clone();
        let req = ai::request(&st.manifest, &entity, &text, hint.as_deref());
        let answer = tokio::task::spawn_blocking(move || provider.call(&scope, &req))
            .await
            .map_err(|_| store_problem(StoreError::Backend("task failed".into())))?
            .map_err(ai_problem)?;
        let mut drafts = ai::drafts(&st.manifest, &entity, &answer.data).map_err(ai_problem)?;
        // A draft the server would refuse to create (period-managed fields, say) is not ready.
        for d in &mut drafts {
            let op = writes::Op::Create {
                data: d.body.clone(),
                key: String::new(),
            };
            if check_server_only(&st.manifest, &entity, &op).is_err() {
                d.errors.push(json!({"path": "", "code": "server_managed",
                    "detail": "this draft sets fields only the server may write"}));
            }
        }
        let ready = drafts.iter().filter(|d| d.errors.is_empty()).count();
        let drafts: Vec<Value> = drafts
            .into_iter()
            .map(|d| json!({"body": d.body, "errors": d.errors}))
            .collect();
        let body = json!({"data": {"drafts": drafts, "ready": ready}, "meta": {"request_id": rid}});
        Ok((StatusCode::OK, Json(body)).into_response())
    };
    finish(&rid, run.await)
}

fn ai_problem(e: ce_provider::ProviderError) -> Problem {
    use ce_provider::ProviderError as E;
    match e {
        E::Unavailable => {
            let mut p = Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "the AI service is unavailable right now",
            );
            p.retryable = true;
            p
        }
        E::Invalid(_) | E::ApprovalRequired => Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "something went wrong on our side",
        ),
        E::Failed(_) => Problem::new(
            StatusCode::BAD_GATEWAY,
            "provider_failed",
            "the AI service could not read that text",
        ),
    }
}
