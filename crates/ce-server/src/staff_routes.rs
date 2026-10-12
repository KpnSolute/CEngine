//! `POST /_auth/pin` (change your own PIN) and `/_staff` (administer staff). They exist when the
//! host's login keeps staff records it can change; otherwise they answer 503. Staff
//! administration needs the admin role (`Issuing::admin_role`, `admin` unless the host says
//! otherwise) and a caller can only grant or touch roles at or below their own.

use super::{authenticate, authenticate_any, finish, parse_object, request_id, AppState, Problem};
use crate::login::Issuing;
use crate::staff::{AdminError, StaffAdmin, StaffInfo};
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use ce_auth::Principal;
use ce_store::Store;
use serde_json::{json, Map, Value};
use std::sync::Arc;

fn problem(e: AdminError) -> Problem {
    match e {
        AdminError::Invalid(m) => Problem::validation(
            m.clone(),
            vec![json!({"path": "", "code": "invalid", "detail": m})],
        ),
        AdminError::Forbidden(m) => Problem::new(StatusCode::FORBIDDEN, "forbidden", m),
        AdminError::WrongPin => Problem::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "the current PIN was not accepted",
        ),
        AdminError::Throttled => {
            let mut p = Problem::new(
                StatusCode::TOO_MANY_REQUESTS,
                "throttled",
                "too many attempts, try again later",
            );
            p.retryable = true;
            p
        }
        AdminError::Missing => Problem::not_found(),
        AdminError::Exists => Problem::new(
            StatusCode::CONFLICT,
            "conflict",
            "that username is already taken",
        ),
        AdminError::ReadOnly => Problem::new(
            StatusCode::CONFLICT,
            "read_only",
            "staff are managed in the staff file on this installation",
        ),
        AdminError::Unavailable => {
            let mut p = Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "staff records are unavailable right now",
            );
            p.retryable = true;
            p
        }
    }
}

fn issuing<S>(st: &AppState<S>, inst: &str) -> Result<Arc<Issuing>, Problem> {
    if inst != st.installation {
        return Err(Problem::not_found());
    }
    match &st.issuing {
        Some(i) if i.login.admin().is_some() => Ok(i.clone()),
        _ => Err(Problem::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "no staff administration is configured",
        )),
    }
}

fn require_admin<S>(
    st: &AppState<S>,
    headers: &HeaderMap,
    inst: &str,
) -> Result<(Arc<Issuing>, Principal), Problem> {
    let i = issuing(st, inst)?;
    let who = authenticate(st, headers)?;
    if who.scope().tenant_id() != i.tenant {
        return Err(Problem::forbidden());
    }
    if !who.has_role(&st.staff_admin_role) {
        return Err(Problem::forbidden());
    }
    Ok((i, who))
}

fn admin(i: &Issuing) -> &dyn StaffAdmin {
    i.login.admin().expect("checked when the route started")
}

fn shown(s: &StaffInfo) -> Value {
    json!({"username": s.username, "roles": s.roles, "active": s.active,
           "must_rotate": s.must_rotate, "pin_updated_at": s.pin_updated_at})
}

fn text<'a>(o: &'a Map<String, Value>, k: &str) -> Result<&'a str, Problem> {
    o.get(k).and_then(Value::as_str).ok_or_else(|| {
        Problem::validation(
            format!("{k} is required"),
            vec![json!({"path": format!("/{k}"), "code": "required"})],
        )
    })
}

fn roles_of(o: &Map<String, Value>) -> Result<Option<Vec<String>>, Problem> {
    let Some(v) = o.get("roles") else {
        return Ok(None);
    };
    v.as_array()
        .and_then(|a| {
            a.iter()
                .map(|r| r.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()
        })
        .map(Some)
        .ok_or_else(|| {
            Problem::validation(
                "roles must be a list of names",
                vec![json!({"path": "/roles", "code": "invalid"})],
            )
        })
}

async fn run<T: Send + 'static>(
    i: Arc<Issuing>,
    f: impl FnOnce(&dyn StaffAdmin) -> Result<T, AdminError> + Send + 'static,
) -> Result<T, Problem> {
    tokio::task::spawn_blocking(move || f(admin(&i)))
        .await
        .map_err(|_| {
            Problem::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "the request could not be completed",
            )
        })?
        .map_err(problem)
}

/// `POST .../_auth/pin` `{"current_pin","new_pin"}`: the signed-in person changes their own PIN.
/// A token issued for a forced change (no roles) may call only this.
pub(crate) async fn change_pin<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id();
    let go = async {
        let i = issuing(&st, &inst)?;
        let who = authenticate_any(&st, &headers)?;
        if who.scope().tenant_id() != i.tenant {
            return Err(Problem::forbidden());
        }
        let o = parse_object(&body)?;
        let (cur, new) = (
            text(&o, "current_pin")?.to_string(),
            text(&o, "new_pin")?.to_string(),
        );
        let sub = who.user_id.clone();
        run(i, move |a| a.change_pin(&sub, &cur, &new)).await?;
        Ok(Json(json!({"data": {"changed": true}, "meta": {"request_id": rid}})).into_response())
    };
    finish(&rid, go.await)
}

/// `GET .../_staff`: every account, never a hash.
pub(crate) async fn staff_list<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let go = async {
        let (i, _) = require_admin(&st, &headers, &inst)?;
        let all = run(i, |a| a.list()).await?;
        let data: Vec<Value> = all.iter().map(shown).collect();
        Ok(Json(json!({"data": data, "meta": {"request_id": rid}})).into_response())
    };
    finish(&rid, go.await)
}

/// `POST .../_staff` `{"username","roles","pin"}`: creates an account that must pick its own PIN
/// at first sign-in.
pub(crate) async fn staff_create<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id();
    let go = async {
        let (i, who) = require_admin(&st, &headers, &inst)?;
        let o = parse_object(&body)?;
        let name = text(&o, "username")?.to_string();
        let pin = text(&o, "pin")?.to_string();
        let roles = roles_of(&o)?.ok_or_else(|| {
            Problem::validation(
                "roles is required",
                vec![json!({"path": "/roles", "code": "required"})],
            )
        })?;
        let actor = who.user_id.clone();
        let held = who.roles.clone();
        let made = run(i, move |a| a.create(&actor, &held, &name, &roles, &pin)).await?;
        Ok((
            StatusCode::CREATED,
            Json(json!({"data": shown(&made), "meta": {"request_id": rid}})),
        )
            .into_response())
    };
    finish(&rid, go.await)
}

/// `PATCH .../_staff/{username}` `{"roles"?,"active"?}`.
pub(crate) async fn staff_update<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, username)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id();
    let go = async {
        let (i, who) = require_admin(&st, &headers, &inst)?;
        let o = parse_object(&body)?;
        let roles = roles_of(&o)?;
        let active = match o.get("active") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                return Err(Problem::validation(
                    "active must be true or false",
                    vec![json!({"path": "/active", "code": "invalid"})],
                ))
            }
        };
        let (actor, held) = (who.user_id.clone(), who.roles.clone());
        let out = run(i, move |a| {
            a.update(&actor, &held, &username, roles.as_deref(), active)
        })
        .await?;
        Ok(Json(json!({"data": shown(&out), "meta": {"request_id": rid}})).into_response())
    };
    finish(&rid, go.await)
}

/// `POST .../_staff/{username}/reset_pin` `{"pin"}`: a temporary PIN; the person picks their own
/// at the next sign-in.
pub(crate) async fn staff_reset_pin<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, username)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let rid = request_id();
    let go = async {
        let (i, who) = require_admin(&st, &headers, &inst)?;
        let o = parse_object(&body)?;
        let pin = text(&o, "pin")?.to_string();
        let (actor, held) = (who.user_id.clone(), who.roles.clone());
        let out = run(i, move |a| a.reset_pin(&actor, &held, &username, &pin)).await?;
        Ok(Json(json!({"data": shown(&out), "meta": {"request_id": rid}})).into_response())
    };
    finish(&rid, go.await)
}
