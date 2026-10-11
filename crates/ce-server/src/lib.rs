//! CEngine HTTP server (draft v1): data endpoints over a [`Store`], with bearer-token auth,
//! role checks from the manifest, RFC 9457 errors and a generated OpenAPI document.
//!
//! Order of checks on every data request: token (401), installation (404), role (403),
//! entity exists (404). A caller without a grant gets 403 for known and unknown entities
//! alike, so entity names do not leak. Tenant comes only from the verified token.
//!
//! Endpoints: `POST /v1/{inst}/{entity}` (Idempotency-Key required), `GET|PATCH|DELETE
//! /v1/{inst}/{entity}/{id}` (If-Match required on writes; PATCH is JSON Merge Patch),
//! `GET /_meta/health/{live,ready}`, `GET /_meta/manifest`, `GET /_meta/openapi`.
//! Not yet: list/query, batch, actions, changesets, events, files; field type validation.

mod openapi;
mod problem;

pub use problem::Problem;

use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use ce_auth::{Principal, Verifier};
use ce_manifest::Manifest;
use ce_policy::{Access, Policy};
use ce_store::{Record, Store, StoreError};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Names the engine owns on every record; callers may never set them.
const RESERVED: [&str; 6] = [
    "id",
    "tenant_id",
    "version",
    "created_at",
    "updated_at",
    "deleted_at",
];
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
        .route("/v1/{inst}/{entity}", axum::routing::post(create::<S>))
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

/// Rejects names the engine owns, fields the template does not declare, and computed
/// fields. `null` is allowed only when `allow_null` (merge patch removes a field).
fn check_fields<S>(
    st: &AppState<S>,
    entity: &str,
    body: &Map<String, Value>,
    allow_null: bool,
) -> Result<(), Problem> {
    let fields = &st.manifest.value["entities"][entity]["fields"];
    let mut errors = Vec::new();
    for (k, v) in body {
        let path = format!("/{k}");
        if RESERVED.contains(&k.as_str()) {
            errors.push(json!({"path": path, "code": "reserved_field"}));
        } else if !fields[k].is_object() {
            errors.push(json!({"path": path, "code": "unknown_field"}));
        } else if fields[k]["computed"].is_string() {
            errors.push(json!({"path": path, "code": "computed_field"}));
        } else if v.is_null() && (!allow_null || fields[k]["required"] == true) {
            let code = if allow_null {
                "required"
            } else {
                "null_not_allowed"
            };
            errors.push(json!({"path": path, "code": code}));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Problem::validation("the body has invalid fields", errors))
    }
}

fn missing_required<S>(
    st: &AppState<S>,
    entity: &str,
    body: &Map<String, Value>,
) -> Result<(), Problem> {
    let errors: Vec<Value> = st.manifest.value["entities"][entity]["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, f)| {
            f["required"] == true && !f["computed"].is_string() && !body.contains_key(k.as_str())
        })
        .map(|(k, _)| json!({"path": format!("/{k}"), "code": "required"}))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Problem::validation("required fields are missing", errors))
    }
}

/// RFC 7396 JSON Merge Patch.
fn merge_patch(target: &mut Value, patch: &Value) {
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
        // The cause goes to logs, never to the caller.
        StoreError::Backend(_) => Problem::new(
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
        let obj = parse_object(&body.map_err(body_problem)?)?;
        check_fields(&st, &entity, &obj, false)?;
        missing_required(&st, &entity, &obj)?;
        let scope = who.scope().clone();
        let readable = st.policy.can_read(&who.roles, &entity);
        let ent = entity.clone();
        let rec = blocking(&st, move |s| {
            s.insert(&scope, &ent, Value::Object(obj), Some(&key))
        })
        .await?;
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
        check_fields(&st, &entity, &patch, true)?;
        let scope = who.scope().clone();
        let readable = st.policy.can_read(&who.roles, &entity);
        let rec = blocking(&st, move |s| {
            let current = s.get(&scope, &entity, &id)?;
            if current.version != expected {
                return Err(StoreError::VersionConflict {
                    current: current.version,
                });
            }
            let mut data = current.data.clone();
            merge_patch(&mut data, &Value::Object(patch));
            s.update(&scope, &entity, &id, expected, data)
        })
        .await?;
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
        let scope = who.scope().clone();
        blocking(&st, move |s| s.delete(&scope, &entity, &id, expected)).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    };
    finish(&rid, run.await)
}
