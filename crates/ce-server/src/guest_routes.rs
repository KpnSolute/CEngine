//! The guest HTTP surface, under `/v1/{inst}/_guest`:
//!
//! - `POST signup`, `login`, `logout`, `reset`, `reset/confirm`: accounts (see [`crate::guest`]).
//! - `GET` and `POST {resource}`: a guest's own rows of a manifest `guest.resources` entry.
//! - `GET {public}`: one value of a manifest `guest.public` entry, no token.
//!
//! And `GET /v1/{inst}/_view/{projection}`: a projection restricted to roles (`roles` on the projection).
//!
//! A guest never names an entity, an owner or a state: the manifest does. Writes go through the same
//! validation, rules and audit as any other write, with the guest as the actor.

use super::*;
use crate::guest::{GuestError, GuestIdentity, GuestService, SUB_PREFIX};
use ce_manifest::snake;

/// Guest accounts as the server holds them: the service, the one tenant they belong to, and the
/// manifest role their tokens carry.
pub struct Guests {
    pub svc: Arc<GuestService>,
    pub tenant: String,
    pub role: String,
}

const MAX_FIELD_CHARS: usize = 2000;
const LIST_LIMIT: usize = 200;
/// Most rows a guest may create per hour.
const WRITES_PER_HOUR: usize = 20;

fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().chars().take(64).collect::<String>())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn guest_problem(e: GuestError) -> Problem {
    match e {
        GuestError::Invalid(field, why) => Problem::validation(
            "a field cannot be used",
            vec![json!({"path": field, "code": "invalid", "detail": why})],
        ),
        GuestError::Exists => Problem::new(
            StatusCode::CONFLICT,
            "email_taken",
            "an account with that email already exists",
        ),
        GuestError::Denied => Problem::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "those credentials were not accepted",
        ),
        GuestError::Throttled => {
            let mut p = Problem::new(
                StatusCode::TOO_MANY_REQUESTS,
                "throttled",
                "too many attempts, try again later",
            );
            p.retryable = true;
            p
        }
        GuestError::Unavailable => {
            let mut p = Problem::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "guest accounts are unavailable right now",
            );
            p.retryable = true;
            p
        }
    }
}

fn guests<S>(st: &AppState<S>, inst: &str) -> Result<Arc<Guests>, Problem> {
    if inst != st.installation {
        return Err(Problem::not_found());
    }
    st.guests.clone().ok_or_else(Problem::not_found)
}

fn str_field(body: &Map<String, Value>, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn issue_token<S>(st: &AppState<S>, g: &Guests, who: &GuestIdentity) -> Result<Value, Problem> {
    // A token issued in the same second as a sign-out would be refused by that cutoff: start one second after it.
    let now = (st.now)().max(g.svc.not_before(&who.sub).map_or(0, |n| n + 1));
    let ttl = g.svc.ttl_secs();
    let token = st
        .verifier
        .sign_token(&who.sub, &g.tenant, std::slice::from_ref(&g.role), now, ttl)
        .map_err(|_| store_problem(StoreError::Backend("cannot sign a token".into())))?;
    Ok(
        json!({"data": {"token": token, "expires_at": now + ttl, "name": who.name, "email": who.email, "profile": who.profile}}),
    )
}

fn credentials(body: Result<Bytes, BytesRejection>) -> Result<Map<String, Value>, Problem> {
    parse_object(&body.map_err(body_problem)?)
}

/// Runs a scrypt-bearing call off the async runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Problem> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| store_problem(StoreError::Backend("task failed".into())))
}

pub(super) async fn signup<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let b = credentials(body)?;
        let (ip, name, email, pw) = (
            client_ip(&headers),
            str_field(&b, "name"),
            str_field(&b, "email"),
            str_field(&b, "password"),
        );
        let svc = g.svc.clone();
        let profile = b.get("profile").cloned().unwrap_or(Value::Null);
        let who = off(move || svc.signup_with_profile(&ip, &name, &email, &pw, &profile))
            .await?
            .map_err(guest_problem)?;
        Ok((StatusCode::CREATED, Json(issue_token(&st, &g, &who)?)).into_response())
    };
    finish(&rid, run.await)
}

pub(super) async fn login<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let b = credentials(body)?;
        let (ip, email, pw) = (
            client_ip(&headers),
            str_field(&b, "email"),
            str_field(&b, "password"),
        );
        let svc = g.svc.clone();
        let who = off(move || svc.login(&ip, &email, &pw))
            .await?
            .map_err(guest_problem)?;
        Ok((StatusCode::OK, Json(issue_token(&st, &g, &who)?)).into_response())
    };
    finish(&rid, run.await)
}

/// The guest behind a bearer token, or a refusal. Staff tokens are refused here: a guest route
/// serves guests only.
fn authenticate_guest<S>(
    st: &AppState<S>,
    g: &Guests,
    headers: &HeaderMap,
) -> Result<Principal, Problem> {
    let who = authenticate(st, headers)?;
    if !who.user_id.starts_with(SUB_PREFIX)
        || !who.has_role(&g.role)
        || who.scope().tenant_id() != g.tenant
    {
        return Err(Problem::forbidden());
    }
    Ok(who)
}

/// The profile questions the manifest declares, in order.
fn questions<S>(st: &AppState<S>) -> Value {
    st.manifest.value["guest"]["profile"].clone()
}

/// `GET .../_guest/onboarding`: what to ask a new guest (no token needed). Empty when nothing is declared.
pub(super) async fn onboarding<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
) -> Response {
    let rid = request_id();
    let run = async {
        guests(&st, &inst)?;
        let q = match questions(&st) {
            Value::Array(a) => Value::Array(a),
            _ => json!([]),
        };
        Ok((StatusCode::OK, Json(json!({"data": {"questions": q}}))).into_response())
    };
    finish(&rid, run.await)
}

fn profile_body<S>(st: &AppState<S>, who: &GuestIdentity) -> Value {
    json!({"data": {"name": who.name, "email": who.email, "profile": who.profile, "questions": match questions(st) { Value::Array(a) => Value::Array(a), _ => json!([]) }}})
}

/// `GET .../_guest/profile`: the signed-in guest's own name, email and onboarding answers.
pub(super) async fn profile_get<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let who = authenticate_guest(&st, &g, &headers)?;
        let svc = g.svc.clone();
        let sub = who.user_id.clone();
        let me = off(move || svc.identity(&sub))
            .await?
            .map_err(guest_problem)?;
        Ok((StatusCode::OK, Json(profile_body(&st, &me))).into_response())
    };
    finish(&rid, run.await)
}

/// `PUT .../_guest/profile` with `{name?, profile}`: replaces the answers (each declared question is re-checked).
pub(super) async fn profile_put<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let who = authenticate_guest(&st, &g, &headers)?;
        let b = credentials(body)?;
        let name = b.get("name").and_then(Value::as_str).map(String::from);
        let profile = b.get("profile").cloned().unwrap_or(Value::Null);
        let svc = g.svc.clone();
        let sub = who.user_id.clone();
        let me = off(move || svc.update_profile(&sub, name.as_deref(), &profile))
            .await?
            .map_err(guest_problem)?;
        Ok((StatusCode::OK, Json(profile_body(&st, &me))).into_response())
    };
    finish(&rid, run.await)
}

pub(super) async fn logout<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let who = authenticate_guest(&st, &g, &headers)?;
        let svc = g.svc.clone();
        let sub = who.user_id.clone();
        off(move || svc.logout(&sub))
            .await?
            .map_err(guest_problem)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    };
    finish(&rid, run.await)
}

pub(super) async fn reset<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let b = credentials(body)?;
        let (ip, email) = (client_ip(&headers), str_field(&b, "email"));
        let svc = g.svc.clone();
        off(move || svc.request_reset(&ip, &email))
            .await?
            .map_err(guest_problem)?;
        // The same answer whether or not the account exists.
        Ok((StatusCode::ACCEPTED, Json(json!({"data": {"sent": true}}))).into_response())
    };
    finish(&rid, run.await)
}

pub(super) async fn reset_confirm<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path(inst): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let b = credentials(body)?;
        let (token, pw) = (str_field(&b, "token"), str_field(&b, "password"));
        let svc = g.svc.clone();
        off(move || svc.confirm_reset(&token, &pw))
            .await?
            .map_err(guest_problem)?;
        Ok((StatusCode::OK, Json(json!({"data": {"reset": true}}))).into_response())
    };
    finish(&rid, run.await)
}

// ---------------------------------------------------------------- resources

fn resource<'m>(m: &'m Manifest, name: &str) -> Option<&'m Value> {
    let r = &m.value["guest"]["resources"][name];
    r.is_object().then_some(r)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}

/// An expression result as JSON (null stays null).
fn to_json(v: ce_expr::Value) -> Value {
    match v {
        ce_expr::Value::Null => Value::Null,
        ce_expr::Value::Bool(b) => json!(b),
        ce_expr::Value::Int(i) => json!(i),
        ce_expr::Value::Dec(d) => json!(d.to_string()),
        ce_expr::Value::Str(t) => json!(t),
        ce_expr::Value::List(l) => Value::Array(l.into_iter().map(to_json).collect()),
    }
}

fn from_json(v: &Value) -> ce_expr::Value {
    match v {
        Value::Bool(b) => ce_expr::Value::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(ce_expr::Value::Int)
            .unwrap_or(ce_expr::Value::Null),
        Value::String(t) => ce_expr::Value::Str(t.clone()),
        _ => ce_expr::Value::Null,
    }
}

fn field_error(path: &str, code: &str, detail: &str) -> Value {
    json!({"path": path, "code": code, "detail": detail})
}

/// Today (UTC) as `YYYY-MM-DD` from unix seconds.
pub(crate) fn iso_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    // Civil-from-days (Howard Hinnant), valid for the whole supported range.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Builds the row to store from what a guest sent, or the field errors that stop it.
pub(crate) fn build_row(
    res: &Value,
    sub: &str,
    sent: &Value,
    now: u64,
    me: Option<&GuestIdentity>,
) -> Result<Map<String, Value>, Vec<Value>> {
    let body = &res["body"];
    let key = s(body, "as");
    let Some(form) = sent.get(key).and_then(Value::as_object) else {
        return Err(vec![field_error(
            key,
            "required",
            &format!("send your answers as `{key}`"),
        )]);
    };
    let allow: Vec<&str> = body["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut errors = Vec::new();
    let mut doc = Map::new();
    for k in &allow {
        match form.get(*k) {
            None | Some(Value::Null) => {}
            Some(Value::String(t)) => {
                if t.chars().count() > MAX_FIELD_CHARS {
                    errors.push(field_error(k, "too_long", "that is too long"));
                } else if t
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
                {
                    errors.push(field_error(
                        k,
                        "invalid",
                        "that has characters that cannot be stored",
                    ));
                } else {
                    doc.insert((*k).to_string(), json!(t.trim()));
                }
            }
            Some(v @ Value::Bool(_)) => {
                doc.insert((*k).to_string(), v.clone());
            }
            Some(v @ Value::Number(n)) if n.is_i64() => {
                doc.insert((*k).to_string(), v.clone());
            }
            Some(_) => errors.push(field_error(k, "invalid", "send text, a number or yes/no")),
        }
    }
    // Anything the guest left out is filled in from their account (name, email, onboarding answers).
    if let (Some(me), Some(defaults)) = (me, body["defaults"].as_object()) {
        for (k, src) in defaults {
            let empty = match doc.get(k) {
                None => true,
                Some(Value::String(t)) => t.is_empty(),
                Some(_) => false,
            };
            if !empty || !allow.contains(&k.as_str()) {
                continue;
            }
            let from = src.as_str().unwrap_or_default();
            let v = match from {
                "$name" => Some(me.name.clone()),
                "$email" => Some(me.email.clone()),
                _ => from
                    .strip_prefix("profile.")
                    .and_then(|q| me.profile.get(q))
                    .and_then(Value::as_str)
                    .map(String::from),
            };
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                doc.insert(k.clone(), json!(v));
            }
        }
    }
    for k in body["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let missing = match doc.get(k) {
            None => true,
            Some(Value::String(t)) => t.is_empty(),
            Some(_) => false,
        };
        if missing {
            errors.push(field_error(k, "required", "this is required"));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let today = iso_date(now);
    let mut env: ce_expr::Env = HashMap::new();
    env.insert("today".into(), ce_expr::Value::Str(today.clone()));
    env.insert(
        "now".into(),
        ce_expr::Value::Int(i64::try_from(now).unwrap_or(0)),
    );
    for k in &allow {
        env.insert(
            snake(k),
            doc.get(*k).map(from_json).unwrap_or(ce_expr::Value::Null),
        );
    }
    if let Some(derive) = body["derive"].as_object() {
        for (k, src) in derive {
            let v = ce_expr::run(src.as_str().unwrap_or(""), &env)
                .map(to_json)
                .unwrap_or(Value::Null);
            env.insert(snake(k), from_json(&v));
            doc.insert(k.clone(), v);
        }
    }
    for rule in body["rules"].as_array().into_iter().flatten() {
        let ok = matches!(
            ce_expr::run(s(rule, "expr"), &env),
            Ok(ce_expr::Value::Bool(true))
        );
        if !ok {
            errors.push(field_error(
                rule["field"].as_str().unwrap_or(key),
                "rule",
                s(rule, "message"),
            ));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    if let Some(st) = body["state"].as_object() {
        doc.insert(
            st["key"].as_str().unwrap_or_default().to_string(),
            json!(st["initial"].as_str().unwrap_or_default()),
        );
    }
    let max = body["max_bytes"].as_u64().unwrap_or(8192) as usize;
    if Value::Object(doc.clone()).to_string().len() > max {
        return Err(vec![field_error(
            key,
            "too_long",
            "that is too much to send at once",
        )]);
    }
    let mut row = Map::new();
    if let Some(f) = res["fixed"].as_object() {
        row.extend(f.clone());
    }
    for k in res["stamp"].as_object().into_iter().flat_map(|m| m.keys()) {
        row.insert(k.clone(), json!(today));
    }
    if let (Some(field), Some(src)) = (res["title_field"].as_str(), res["title"].as_str()) {
        if let Ok(ce_expr::Value::Str(t)) = ce_expr::run(src, &env) {
            row.insert(
                field.to_string(),
                json!(t.chars().take(200).collect::<String>()),
            );
        }
    }
    row.insert(s(res, "owner_field").to_string(), json!(sub));
    match body["columns"].as_object() {
        // Column mode: every form key lands in its own field of the entity.
        Some(cols) => {
            for (k, target) in cols {
                if let (Some(v), Some(t)) = (doc.get(k), target.as_str()) {
                    row.insert(t.to_string(), v.clone());
                }
            }
        }
        None => {
            row.insert(s(body, "field").to_string(), Value::Object(doc));
        }
    }
    Ok(row)
}

/// Column mode: the guest's own form keys read back from the entity's fields, the state shown through
/// `status_map` as `status`, and the note. Nothing else of the row leaves the server.
fn column_view(res: &Value, cols: &Map<String, Value>, rec: &Record) -> Value {
    let body = &res["body"];
    let mut out = Map::new();
    out.insert("id".into(), json!(rec.id));
    if let Some(sf) = body["state_field"].as_str() {
        let raw = rec.data.get(sf).and_then(Value::as_str).unwrap_or_default();
        let shown = body["status_map"][raw].as_str().unwrap_or(raw);
        out.insert("status".into(), json!(shown));
    }
    if let Some(nf) = body["note_field"].as_str() {
        if let Some(note) = rec
            .data
            .get(nf)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.insert("note".into(), json!(note));
        }
    }
    if let Some(f) = res["created_at_field"].as_str() {
        if let Some(d) = rec.data.get(f) {
            out.insert("created_at".into(), d.clone());
        }
    }
    let form: Map<String, Value> = cols
        .iter()
        .filter_map(|(k, target)| {
            let v = rec.data.get(target.as_str()?)?;
            (!v.is_null() && v != &json!("")).then(|| (k.clone(), v.clone()))
        })
        .collect();
    out.insert(s(body, "as").to_string(), Value::Object(form));
    Value::Object(out)
}

/// A stored row as the guest sees it: its id, its state and note, its date, and only the
/// allowed and derived keys of its body, never anything staff added.
pub(crate) fn guest_view(res: &Value, rec: &Record) -> Value {
    let body = &res["body"];
    if let Some(cols) = body["columns"].as_object() {
        return column_view(res, cols, rec);
    }
    let stored = rec
        .data
        .get(s(body, "field"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    out.insert("id".into(), json!(rec.id));
    if let Some(st) = body["state"].as_object() {
        let k = st["key"].as_str().unwrap_or_default();
        let v = stored
            .get(k)
            .cloned()
            .unwrap_or_else(|| st["initial"].clone());
        out.insert(k.to_string(), v);
    }
    if let Some(n) = body["note_key"].as_str() {
        if let Some(note) = stored
            .get(n)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            out.insert(n.to_string(), json!(note));
        }
    }
    if let Some(f) = res["created_at_field"].as_str() {
        if let Some(d) = rec.data.get(f) {
            out.insert("created_at".into(), d.clone());
        }
    }
    let shown: Vec<&str> = body["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .chain(
            body["derive"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.keys().map(String::as_str)),
        )
        .collect();
    let form: Map<String, Value> = shown
        .into_iter()
        .filter_map(|k| stored.get(k).map(|v| (k.to_string(), v.clone())))
        .collect();
    out.insert(s(body, "as").to_string(), Value::Object(form));
    Value::Object(out)
}

/// What a guest may see of a feed: only the declared fields of rows that pass `where`, in `order_by`
/// order. Rows are read from the store first (at most `scan`), then filtered here, because the
/// keys live inside a json body the store cannot filter on.
pub(crate) fn feed_view(feed: &Value, recs: &[Record], now: u64) -> Vec<Value> {
    let keys: Vec<&str> = feed["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let body_field = s(feed, "body_field");
    let today = iso_date(now);
    let mut rows: Vec<Map<String, Value>> = Vec::new();
    for rec in recs {
        let body = if body_field.is_empty() {
            rec.data.as_object()
        } else {
            rec.data.get(body_field).and_then(Value::as_object)
        };
        let mut env: ce_expr::Env = HashMap::new();
        env.insert("today".into(), ce_expr::Value::Str(today.clone()));
        env.insert(
            "now".into(),
            ce_expr::Value::Int(i64::try_from(now).unwrap_or(0)),
        );
        for k in &keys {
            env.insert(
                snake(k),
                body.and_then(|b| b.get(*k))
                    .map(from_json)
                    .unwrap_or(ce_expr::Value::Null),
            );
        }
        if !matches!(
            ce_expr::run(s(feed, "where"), &env),
            Ok(ce_expr::Value::Bool(true))
        ) {
            continue;
        }
        let mut row = Map::new();
        row.insert("id".into(), json!(rec.id));
        for (name, spec) in feed["fields"].as_object().into_iter().flatten() {
            let v = match (spec["from"].as_str(), spec["expr"].as_str()) {
                (Some(k), _) => body.and_then(|b| b.get(k)).cloned().unwrap_or(Value::Null),
                (_, Some(e)) => ce_expr::run(e, &env).map(to_json).unwrap_or(Value::Null),
                _ => Value::Null,
            };
            if !v.is_null() && v != json!("") {
                row.insert(name.clone(), v);
            }
        }
        rows.push(row);
    }
    if let Some(o) = feed["order_by"].as_str() {
        let (field, desc) = match o.strip_prefix('-') {
            Some(f) => (f, true),
            None => (o, false),
        };
        let key = |r: &Map<String, Value>| {
            r.get(field)
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_default()
        };
        rows.sort_by(|a, b| {
            if desc {
                key(b).cmp(&key(a))
            } else {
                key(a).cmp(&key(b))
            }
        });
    }
    rows.truncate(feed["limit"].as_u64().unwrap_or(200).min(500) as usize);
    rows.into_iter().map(Value::Object).collect()
}

fn owner_filter(res: &Value, sub: &str) -> Value {
    let mut all = vec![json!({"field": s(res, "owner_field"), "op": "eq", "value": sub})];
    for (k, v) in res["fixed"].as_object().into_iter().flatten() {
        all.push(json!({"field": k, "op": "eq", "value": v}));
    }
    json!({"and": all})
}

/// `GET .../_guest/{name}`: a guest's own rows, or a public value. `POST`: a new row of a resource.
pub(super) async fn guest_get<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        if let Some(p) = st.manifest.value["guest"]["public"]
            .get(&name)
            .filter(|p| p.is_object())
        {
            let scope = st
                .verifier
                .public_scope(&g.tenant)
                .ok_or_else(Problem::not_found)?;
            let mut req = ListRequest {
                select: None,
                filter: Some(
                    json!({"field": s(&p["match"], "field"), "op": "eq", "value": s(&p["match"], "value")}),
                ),
                search: None,
                sort: vec![],
                limit: Some(1),
                after: None,
            };
            req.select = Some(vec![s(p, "value_field").to_string()]);
            let entity = s(p, "entity").to_string();
            let field = s(p, "value_field").to_string();
            let page = blocking(&st, move |x| x.list(&scope, &entity, &req)).await?;
            let value = page
                .records
                .first()
                .and_then(|r| r.data.get(&field).cloned())
                .unwrap_or_else(|| json!({}));
            let mut res = (StatusCode::OK, Json(json!({"data": value}))).into_response();
            res.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=30"),
            );
            return Ok(res);
        }
        if let Some(feed) = st.manifest.value["guest"]["feeds"]
            .get(&name)
            .filter(|f| f.is_object())
            .cloned()
        {
            let who = authenticate_guest(&st, &g, &headers)?;
            // A switch managers control: while it is off the feed lists nothing.
            if let Some(sw) = feed.get("enabled_by").filter(|v| v.is_object()) {
                let scope = who.scope().clone();
                let req = ListRequest {
                    select: None,
                    filter: Some(
                        json!({"field": s(&sw["match"], "field"), "op": "eq", "value": s(&sw["match"], "value")}),
                    ),
                    search: None,
                    sort: vec![],
                    limit: Some(1),
                    after: None,
                };
                let (entity, vf, key) = (
                    s(sw, "entity").to_string(),
                    s(sw, "value_field").to_string(),
                    s(sw, "key").to_string(),
                );
                let page = blocking(&st, move |x| x.list(&scope, &entity, &req)).await?;
                let off_now = page
                    .records
                    .first()
                    .and_then(|r| r.data.get(&vf))
                    .is_some_and(|v| v[key.as_str()] == json!(false));
                if off_now {
                    return Ok((
                        StatusCode::OK,
                        Json(json!({"data": [], "meta": {"request_id": rid, "enabled": false}})),
                    )
                        .into_response());
                }
            }
            let all: Vec<Value> = feed["fixed"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, v)| json!({"field": k, "op": "eq", "value": v}))
                .collect();
            let sort = feed["sort"]
                .as_str()
                .map(|t| match t.strip_prefix('-') {
                    Some(f) => SortKey {
                        field: f.to_string(),
                        dir: SortDir::Desc,
                    },
                    None => SortKey {
                        field: t.to_string(),
                        dir: SortDir::Asc,
                    },
                })
                .into_iter()
                .collect();
            let req = ListRequest {
                select: None,
                filter: if all.is_empty() {
                    None
                } else {
                    Some(json!({"and": all}))
                },
                search: None,
                sort,
                limit: Some(feed["scan"].as_u64().unwrap_or(500).min(1000) as usize),
                after: None,
            };
            let scope = who.scope().clone();
            let entity = s(&feed, "entity").to_string();
            let page = blocking(&st, move |x| x.list(&scope, &entity, &req)).await?;
            let data = feed_view(&feed, &page.records, (st.now)());
            return Ok((
                StatusCode::OK,
                Json(json!({"data": data, "meta": {"request_id": rid}})),
            )
                .into_response());
        }
        let res = resource(&st.manifest, &name)
            .ok_or_else(Problem::not_found)?
            .clone();
        let who = authenticate_guest(&st, &g, &headers)?;
        let sort = res["sort"]
            .as_str()
            .map(|t| match t.strip_prefix('-') {
                Some(f) => SortKey {
                    field: f.to_string(),
                    dir: SortDir::Desc,
                },
                None => SortKey {
                    field: t.to_string(),
                    dir: SortDir::Asc,
                },
            })
            .into_iter()
            .collect();
        let req = ListRequest {
            select: None,
            filter: Some(owner_filter(&res, &who.user_id)),
            search: None,
            sort,
            limit: Some(LIST_LIMIT),
            after: None,
        };
        let scope = who.scope().clone();
        let entity = s(&res, "entity").to_string();
        let page = blocking(&st, move |x| x.list(&scope, &entity, &req)).await?;
        let data: Vec<Value> = page.records.iter().map(|r| guest_view(&res, r)).collect();
        Ok((
            StatusCode::OK,
            Json(json!({"data": data, "meta": {"request_id": rid}})),
        )
            .into_response())
    };
    finish(&rid, run.await)
}

pub(super) async fn guest_post<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let rid = request_id();
    let run = async {
        let g = guests(&st, &inst)?;
        let res = resource(&st.manifest, &name)
            .ok_or_else(Problem::not_found)?
            .clone();
        let who = authenticate_guest(&st, &g, &headers)?;
        let sent = Value::Object(parse_object(&body.map_err(body_problem)?)?);
        if !g.svc.allow_write(&who.user_id, WRITES_PER_HOUR) {
            return Err(guest_problem(GuestError::Throttled));
        }
        let entity = s(&res, "entity").to_string();
        let me = {
            let (svc, sub) = (g.svc.clone(), who.user_id.clone());
            off(move || svc.identity(&sub)).await?.ok()
        };
        let mut row = build_row(&res, &who.user_id, &sent, (st.now)(), me.as_ref())
            .map_err(|errors| Problem::validation("the request cannot be accepted", errors))?;
        validate_body(&st, &entity, &row, ce_validate::Mode::Create)?;
        prepare_create(&st.manifest, &entity, &mut row);
        let key = header_str(&headers, "idempotency-key")?
            .map(String::from)
            .unwrap_or_else(request_id);
        let ctx = invariant_context(&st, &who);
        let rec = write(
            &st,
            who.scope().clone(),
            ctx,
            entity,
            writes::Op::Create { data: row, key },
            &rid,
        )
        .await?
        .ok_or_else(|| store_problem(StoreError::Backend("no record".into())))?;
        Ok((
            StatusCode::CREATED,
            Json(json!({"data": guest_view(&res, &rec), "meta": {"request_id": rid}})),
        )
            .into_response())
    };
    finish(&rid, run.await)
}

// ---------------------------------------------------------------- role-restricted projections

/// `GET /v1/{inst}/_view/{projection}`: a projection with `roles`, for signed-in callers who hold
/// one. Same fields, filter and paging rules as a public projection; the tenant is the caller's.
pub(super) async fn view_read<S: Store + Send + Sync + 'static>(
    State(st): State<AppState<S>>,
    Path((inst, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let rid = request_id();
    let run = async {
        let who = authenticate(&st, &headers)?;
        let proj = st.manifest.value["projections"][name.as_str()].clone();
        let allowed: Vec<&str> = proj["roles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if inst != st.installation || !proj.is_object() || allowed.is_empty() {
            return Err(Problem::not_found());
        }
        if !allowed.iter().any(|r| who.has_role(r)) {
            return Err(Problem::forbidden());
        }
        let entity = s(&proj, "entity").to_string();
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
        req.select = Some(fields.clone());
        req.filter = filter;
        req.limit = Some(
            q.get("limit")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(50)
                .clamp(1, 100),
        );
        let scope = who.scope().clone();
        let page = blocking(&st, move |x| x.list(&scope, &entity, &req)).await?;
        let data: Vec<Value> = page
            .records
            .iter()
            .map(|r| {
                Value::Object(
                    fields
                        .iter()
                        .filter_map(|f| r.data.get(f).map(|v| (f.clone(), v.clone())))
                        .collect(),
                )
            })
            .collect();
        Ok((
            StatusCode::OK,
            Json(json!({"data": data, "meta": {"request_id": rid}})),
        )
            .into_response())
    };
    finish(&rid, run.await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn br(
        res: &Value,
        sub: &str,
        sent: &Value,
        now: u64,
    ) -> Result<Map<String, Value>, Vec<Value>> {
        build_row(res, sub, sent, now, None)
    }

    /// The MJCC-shaped request: a form that lands in the columns of its own entity.
    fn col_res() -> Value {
        json!({
            "entity": "catering_request", "owner_field": "requester",
            "stamp": {"request_date": "today"}, "created_at_field": "request_date",
            "body": {
                "as": "form",
                "columns": {"originator": "originator", "dept": "dept", "ext": "ext", "eventDate": "event_date",
                            "location": "location", "theme": "theme", "food": "food", "sackLunch": "sack_lunch"},
                "allow": ["originator", "dept", "ext", "eventDate", "location", "theme", "food", "sackLunch"],
                "required": ["originator", "dept", "eventDate", "location"],
                "defaults": {"originator": "$name", "dept": "profile.dept", "ext": "profile.ext"},
                "rules": [{"expr": "event_date >= today", "field": "eventDate", "message": "The event date has passed."}],
                "state_field": "status", "note_field": "note",
                "status_map": {"submitted": "pending", "under_review": "pending", "approved": "pending", "ready": "ready", "denied": "denied"}
            }
        })
    }

    fn me() -> GuestIdentity {
        GuestIdentity {
            sub: "guest:abc".into(),
            name: "Tanya Brooks".into(),
            email: "tb@example.org".into(),
            profile: json!({"dept": "Admissions", "ext": "2210"})
                .as_object()
                .unwrap()
                .clone(),
        }
    }

    #[test]
    fn column_mode_writes_entity_fields_and_fills_the_form_from_the_profile() {
        // The guest sends only what is new; name, department and extension come from onboarding.
        let sent = json!({"form": {"eventDate": "2026-12-10", "location": "Main lobby", "theme": "Open house", "food": "Finger foods", "sackLunch": true}});
        let row = build_row(&col_res(), "guest:abc", &sent, NOW, Some(&me())).unwrap();
        assert_eq!(row["requester"], "guest:abc");
        assert_eq!(row["originator"], "Tanya Brooks");
        assert_eq!(
            (row["dept"].as_str(), row["ext"].as_str()),
            (Some("Admissions"), Some("2210"))
        );
        assert_eq!(
            (row["event_date"].as_str(), row["sack_lunch"].as_bool()),
            (Some("2026-12-10"), Some(true))
        );
        assert_eq!(row["request_date"], "2026-11-03");
        assert!(
            !row.contains_key("data") && !row.contains_key("status") && !row.contains_key("note")
        );
        // What the guest does send wins over the profile.
        let own =
            json!({"form": {"dept": "Academics", "eventDate": "2026-12-10", "location": "Gym"}});
        let row = build_row(&col_res(), "guest:abc", &own, NOW, Some(&me())).unwrap();
        assert_eq!(row["dept"], "Academics");
        // Without an account to read from, a missing required field is refused as usual.
        let e = build_row(&col_res(), "guest:abc", &own, NOW, None).unwrap_err();
        assert!(e
            .iter()
            .any(|x| x["path"] == "originator" && x["code"] == "required"));
    }

    #[test]
    fn column_mode_ignores_server_owned_columns_a_guest_tries_to_send() {
        let sent = json!({"form": {"originator": "X", "dept": "D", "eventDate": "2026-12-10", "location": "L", "status": "approved", "approval_1_by": "mia", "requester": "guest:other", "note": "free"}});
        let row = build_row(&col_res(), "guest:abc", &sent, NOW, None).unwrap();
        assert_eq!(row["requester"], "guest:abc");
        for k in ["status", "approval_1_by", "note"] {
            assert!(!row.contains_key(k), "{k}");
        }
        let past = json!({"form": {"originator": "X", "dept": "D", "eventDate": "2026-11-01", "location": "L"}});
        let e = build_row(&col_res(), "guest:abc", &past, NOW, None).unwrap_err();
        assert!(e
            .iter()
            .any(|x| x["detail"] == "The event date has passed."));
    }

    #[test]
    fn the_column_view_maps_the_state_to_what_a_guest_should_see() {
        let rec = |status: &str| Record {
            id: "r1".into(),
            entity: "catering_request".into(),
            version: 4,
            data: json!({"requester": "guest:abc", "originator": "Tanya", "dept": "Admissions", "event_date": "2026-12-10", "location": "Lobby",
                "status": status, "note": "Kitchen is closed", "approval_1_by": "mia", "approval_2_by": "max", "decided_by": "max",
                "request_date": "2026-11-03", "theme": null, "food": ""}),
        };
        for (state, shown) in [
            ("submitted", "pending"),
            ("under_review", "pending"),
            ("approved", "pending"),
            ("ready", "ready"),
            ("denied", "denied"),
        ] {
            assert_eq!(
                guest_view(&col_res(), &rec(state))["status"],
                shown,
                "{state}"
            );
        }
        let v = guest_view(&col_res(), &rec("denied"));
        assert_eq!(v["note"], "Kitchen is closed");
        assert_eq!(v["created_at"], "2026-11-03");
        assert_eq!(
            v["form"],
            json!({"originator": "Tanya", "dept": "Admissions", "eventDate": "2026-12-10", "location": "Lobby"})
        );
        let text = v.to_string();
        assert!(
            !text.contains("mia")
                && !text.contains("max")
                && !text.contains("guest:abc")
                && !text.contains("approval")
        );
    }

    #[test]
    fn a_feed_over_plain_columns_lists_only_declared_fields() {
        let feed = json!({
            "entity": "catering_request", "keys": ["status", "event_date", "theme", "dept", "location"],
            "where": "event_date >= today and (status == 'approved' or status == 'ready')",
            "fields": {"title": {"expr": "when(theme != '', theme, dept)"}, "eventDate": {"from": "event_date"}, "location": {"from": "location"}, "status": {"from": "status"}},
            "order_by": "eventDate"
        });
        let rec = |id: &str, status: &str, date: &str| Record {
            id: id.into(),
            entity: "catering_request".into(),
            version: 1,
            data: json!({"status": status, "event_date": date, "dept": "Academics", "theme": "Graduation", "location": "Gym", "food": "secret", "requester": "guest:x", "originator": "Sam"}),
        };
        let v = feed_view(
            &feed,
            &[
                rec("b", "approved", "2026-12-20"),
                rec("a", "ready", "2026-11-10"),
                rec("c", "submitted", "2026-11-11"),
                rec("d", "approved", "2026-11-01"),
            ],
            NOW,
        );
        assert_eq!(
            v.iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            v[0],
            json!({"id": "a", "title": "Graduation", "eventDate": "2026-11-10", "location": "Gym", "status": "ready"})
        );
        assert!(!serde_json::to_string(&v).unwrap().contains("secret"));
    }

    fn res() -> Value {
        json!({
            "entity": "log", "owner_field": "created_by",
            "fixed": {"entry_type": "food_request"},
            "stamp": {"business_date": "today"},
            "created_at_field": "business_date",
            "title_field": "title", "title": "concat('Food request: ', dept)",
            "body": {
                "field": "data", "as": "form",
                "allow": ["originator", "dept", "eventDate", "location", "food", "drinks", "other", "sackLunch"],
                "required": ["originator", "dept", "eventDate", "location"],
                "derive": {"meal": "when(sack_lunch, 'sack lunch', 'plated')"},
                "rules": [
                    {"expr": "event_date >= today", "field": "eventDate", "message": "The event date has passed."},
                    {"expr": "food != '' or drinks != '' or other != ''", "field": "food", "message": "Ask for something."}
                ],
                "state": {"key": "status", "initial": "submitted"},
                "note_key": "note"
            }
        })
    }

    fn form(extra: Value) -> Value {
        let mut f = json!({"originator": "Jo", "dept": "Wellness", "eventDate": "2026-12-10", "location": "Room 4", "food": "Pizza", "drinks": "", "other": "", "sackLunch": false});
        if let (Some(a), Some(b)) = (f.as_object_mut(), extra.as_object()) {
            a.extend(b.clone());
        }
        json!({"form": f})
    }

    // 2026-11-03 00:00 UTC
    const NOW: u64 = 1_793_664_000;

    #[test]
    fn dates_convert() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(NOW), "2026-11-03");
        assert_eq!(iso_date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn a_good_request_becomes_an_owned_stamped_row() {
        let row = br(&res(), "guest:abc", &form(json!({})), NOW).unwrap();
        assert_eq!(row["created_by"], "guest:abc");
        assert_eq!(row["entry_type"], "food_request");
        assert_eq!(row["business_date"], "2026-11-03");
        assert_eq!(row["title"], "Food request: Wellness");
        let d = &row["data"];
        assert_eq!(d["status"], "submitted");
        assert_eq!(d["meal"], "plated");
        assert_eq!(d["location"], "Room 4");
    }

    #[test]
    fn derived_values_follow_the_form() {
        let sack = br(&res(), "guest:abc", &form(json!({"sackLunch": true})), NOW).unwrap();
        assert_eq!(sack["data"]["meal"], "sack lunch");
    }

    #[test]
    fn a_guest_cannot_set_status_note_owner_or_unknown_keys() {
        let sent = form(
            json!({"status": "approved", "note": "ok", "decidedBy": "boss", "created_by": "guest:other", "meal": "hacked"}),
        );
        let row = br(&res(), "guest:abc", &sent, NOW).unwrap();
        assert_eq!(row["created_by"], "guest:abc");
        let d = row["data"].as_object().unwrap();
        assert_eq!(d["status"], "submitted");
        assert!(
            !d.contains_key("note")
                && !d.contains_key("decidedBy")
                && !d.contains_key("created_by")
        );
        assert_eq!(d["meal"], "plated"); // a derived key a guest sends is replaced by the server's value
    }

    #[test]
    fn rules_and_required_fields_stop_a_request_with_a_reason() {
        let past = br(
            &res(),
            "guest:abc",
            &form(json!({"eventDate": "2026-11-02"})),
            NOW,
        )
        .unwrap_err();
        assert!(past
            .iter()
            .any(|e| e["path"] == "eventDate" && e["detail"] == "The event date has passed."));
        let nothing = br(&res(), "guest:abc", &form(json!({"food": ""})), NOW).unwrap_err();
        assert!(nothing.iter().any(|e| e["detail"] == "Ask for something."));
        let blank = br(
            &res(),
            "guest:abc",
            &form(json!({"location": "  ", "dept": ""})),
            NOW,
        )
        .unwrap_err();
        assert!(blank
            .iter()
            .any(|e| e["path"] == "location" && e["code"] == "required"));
        assert!(blank.iter().any(|e| e["path"] == "dept"));
        assert!(br(&res(), "guest:abc", &json!({}), NOW).is_err());
    }

    #[test]
    fn odd_values_are_refused() {
        let nested = br(&res(), "guest:abc", &form(json!({"food": {"a": 1}})), NOW).unwrap_err();
        assert!(nested.iter().any(|e| e["path"] == "food"));
        let long = br(
            &res(),
            "guest:abc",
            &form(json!({"food": "x".repeat(2001)})),
            NOW,
        )
        .unwrap_err();
        assert!(long.iter().any(|e| e["code"] == "too_long"));
    }

    fn feed() -> Value {
        json!({
            "entity": "log", "fixed": {"entry_type": "food_request"}, "body_field": "data",
            "keys": ["status", "eventDate", "theme", "dept", "location"],
            "where": "event_date >= today and coalesce(status, 'submitted') != 'declined'",
            "fields": {
                "title": {"expr": "when(theme != '', theme, dept)"},
                "eventDate": {"from": "eventDate"},
                "location": {"from": "location"},
                "status": {"expr": "coalesce(status, 'submitted')"}
            },
            "order_by": "eventDate", "limit": 3
        })
    }

    fn rec(id: &str, data: Value) -> Record {
        Record {
            id: id.into(),
            entity: "log".into(),
            version: 1,
            data: json!({"created_by": "guest:secret", "data": data}),
        }
    }

    #[test]
    fn a_feed_shows_only_declared_fields_of_matching_rows_in_order() {
        let recs = vec![
            rec(
                "late",
                json!({"status": "approved", "eventDate": "2026-12-20", "dept": "Wellness", "location": "Gym", "food": "secret menu", "originator": "Jo"}),
            ),
            rec(
                "past",
                json!({"status": "approved", "eventDate": "2026-11-02", "dept": "Old"}),
            ),
            rec(
                "no",
                json!({"status": "declined", "eventDate": "2026-12-01", "dept": "Declined"}),
            ),
            rec(
                "soon",
                json!({"eventDate": "2026-11-10", "theme": "Open house", "dept": "Family", "location": ""}),
            ),
            rec("bare", json!({"status": "submitted"})),
        ];
        let v = feed_view(&feed(), &recs, NOW);
        assert_eq!(v.len(), 2);
        assert_eq!(
            v[0],
            json!({"id": "soon", "title": "Open house", "eventDate": "2026-11-10", "status": "submitted"})
        );
        assert_eq!(
            v[1],
            json!({"id": "late", "title": "Wellness", "eventDate": "2026-12-20", "location": "Gym", "status": "approved"})
        );
        let text = serde_json::to_string(&v).unwrap();
        assert!(!text.contains("secret") && !text.contains("Jo") && !text.contains("originator"));
    }

    #[test]
    fn a_feed_honours_its_limit_and_descending_order() {
        let mut f = feed();
        f["limit"] = json!(2);
        f["order_by"] = json!("-eventDate");
        let recs: Vec<Record> = (10..15)
            .map(|d| {
                rec(
                    &format!("r{d}"),
                    json!({"eventDate": format!("2026-11-{d}"), "dept": "D"}),
                )
            })
            .collect();
        let v = feed_view(&f, &recs, NOW);
        assert_eq!(
            v.iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["r14", "r13"]
        );
    }

    #[test]
    fn the_view_shows_state_note_date_and_only_allowed_keys() {
        let rec = Record {
            id: "r1".into(),
            entity: "log".into(),
            version: 3,
            data: json!({"business_date": "2026-11-03", "created_by": "guest:abc", "severity": "info",
                "data": {"originator": "Jo", "dept": "W", "status": "declined", "note": "No kitchen that day", "decidedBy": "boss", "decidedOn": "2026-11-04", "meal": "plated", "secret": "x"}}),
        };
        let v = guest_view(&res(), &rec);
        assert_eq!(v["id"], "r1");
        assert_eq!(v["status"], "declined");
        assert_eq!(v["note"], "No kitchen that day");
        assert_eq!(v["created_at"], "2026-11-03");
        assert_eq!(v["form"]["originator"], "Jo");
        assert_eq!(v["form"]["meal"], "plated");
        let form = v["form"].as_object().unwrap();
        assert!(
            !form.contains_key("decidedBy")
                && !form.contains_key("secret")
                && !form.contains_key("status")
        );
        assert!(v.get("created_by").is_none());
    }
}
