//! Named actions: `POST /v1/{inst}/actions/{name}` runs the manifest's `effect` list against
//! one record in a single transaction. Effects run in order, each seeing the result of the one
//! before: `set` writes fields from expressions (rules, computed fields and freezes apply as for
//! any write), `transition` moves the record along its state graph, `create` makes a record of
//! another entity, `carry` copies every record that points at this one into new records
//! (period rollover), `update` changes a record of another entity that one of this record's fields
//! points at, `emit` names an event. Any effect may carry a `when` expression and is skipped
//! unless it is true. Expressions read the record's fields, `input.*`,
//! `actor.*`, `now` and `today`. Every change leaves an audit row when the `audit` tool is used.
//! Running an action is its own permission (`roles.*.actions`), so a role that may run it need
//! not write the entity directly.

use crate::writes::{self, Env, Failure, Op, Outcome};
use crate::{Problem, Record};
use axum::http::StatusCode;
use ce_store::{ListRequest, Store, StoreError};
use ce_validate::FieldError;
use serde_json::{json, Map, Value};

/// Most records one `carry` effect copies, so a single request stays a bounded transaction.
const MAX_CARRY: usize = 5000;
const CARRY_PAGE: usize = 250;

pub(crate) struct Run<'a> {
    pub name: &'a str,
    pub id: &'a str,
    pub expected: u64,
    pub input: &'a Map<String, Value>,
    pub key: &'a str,
}

pub(crate) struct Done {
    pub record: Record,
    pub created: Vec<Value>,
    pub emitted: Vec<String>,
}

enum Stop {
    Problem(Problem),
    Fail(Failure),
}

impl From<StoreError> for Stop {
    fn from(e: StoreError) -> Self {
        Stop::Fail(Failure::Store(e))
    }
}

impl From<Failure> for Stop {
    fn from(f: Failure) -> Self {
        Stop::Fail(f)
    }
}

impl From<Problem> for Stop {
    fn from(p: Problem) -> Self {
        Stop::Problem(p)
    }
}

pub(crate) fn exists(m: &ce_manifest::Manifest, name: &str) -> bool {
    m.value["actions"][name].is_object()
}

fn wrong_state(detail: String) -> Stop {
    Stop::Problem(Problem::new(
        StatusCode::CONFLICT,
        "invalid_transition",
        detail,
    ))
}

fn rules(path: String, detail: String) -> Stop {
    Stop::Fail(Failure::Rules(vec![FieldError {
        path,
        code: "action_failed",
        detail,
    }]))
}

fn check_input(action: &Value, input: &Map<String, Value>) -> Result<(), Stop> {
    let none = Map::new();
    let defs = action["input"].as_object().unwrap_or(&none);
    let mut errors = Vec::new();
    for k in input.keys().filter(|k| !defs.contains_key(*k)) {
        errors.push(json!({"path": format!("/input/{k}"), "code": "unknown_field", "detail": "no such input"}));
    }
    for (k, def) in defs {
        match input.get(k) {
            None | Some(Value::Null) if def["required"] == true => {
                errors.push(json!({"path": format!("/input/{k}"), "code": "required", "detail": "this input is required"}));
            }
            Some(v) if !v.is_null() => {
                if let Some(e) = ce_validate::check_value(k, def, v) {
                    errors.push(
                        json!({"path": format!("/input/{k}"), "code": e.code, "detail": e.detail}),
                    );
                }
            }
            _ => {}
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Stop::Problem(Problem::validation(
            "the action input is invalid",
            errors,
        )))
    }
}

/// The value of one expression, shaped for `target`.
#[allow(clippy::too_many_arguments)]
fn eval(
    tx: &dyn Store,
    env: &Env,
    action: &Value,
    entity: &str,
    rec: &Record,
    input: &Map<String, Value>,
    src: &str,
    name: &str,
    target: &Value,
) -> Result<Option<Value>, Stop> {
    let none = Map::new();
    let defs = action["input"].as_object().unwrap_or(&none);
    let data = rec.data.as_object().unwrap_or(&none);
    let out = writes::with_resolver(tx, env, entity, Some(&rec.id), |res| {
        ce_validate::eval_effect(
            env.manifest,
            entity,
            data,
            env.ctx,
            input,
            defs,
            res,
            src,
            name,
            target,
        )
    })?;
    out.map_err(|e| rules(format!("/{name}"), e))
}

/// `carry`: every live record of `entity` that points at the action's record through `via` is
/// copied into a new record. The `values` expressions read the source row's fields (stored and
/// computed) and `input.*`, and must set `via` to somewhere else. A row whose copy is refused
/// (a duplicate, a frozen target, a rule) refuses the whole action. `requires`, when given, is
/// read against the action's own record and must be true.
#[allow(clippy::too_many_arguments)]
fn carry(
    tx: &dyn Store,
    env: &Env,
    action: &Value,
    rec: &Record,
    r: &Run,
    i: usize,
    c: &Map<String, Value>,
    done: &mut Done,
) -> Result<(), Stop> {
    let m = env.manifest;
    let entity = action["entity"].as_str().unwrap_or_default();
    let ce = c["entity"].as_str().unwrap_or_default();
    let via = c["via"].as_str().unwrap_or_default();
    let cent = &m.value["entities"][ce];
    if let Some(req) = c.get("requires").and_then(Value::as_str) {
        let ok = eval(
            tx,
            env,
            action,
            entity,
            rec,
            r.input,
            req,
            "requires",
            &json!({"type": "boolean"}),
        )?;
        if ok != Some(Value::Bool(true)) {
            return Err(wrong_state(format!(
                "`{}` cannot run: the record does not meet `{req}`",
                r.name
            )));
        }
    }
    // Read every source row first, so the copies made below are never read back.
    let mut rows = Vec::new();
    let mut after = None;
    loop {
        let page = tx.list(
            env.scope,
            ce,
            &ListRequest {
                select: None,
                filter: Some(json!({"field": via, "op": "eq", "value": rec.id})),
                search: None,
                sort: Vec::new(),
                limit: Some(CARRY_PAGE),
                after,
            },
        )?;
        rows.extend(page.records);
        if rows.len() > MAX_CARRY {
            return Err(rules(
                format!("/effect/{i}/carry"),
                format!("more than {MAX_CARRY} records to carry"),
            ));
        }
        match page.next {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    for src in rows {
        let mut data = Map::new();
        for (field, expr_src) in c["values"].as_object().into_iter().flatten() {
            let v = eval(
                tx,
                env,
                action,
                ce,
                &src,
                r.input,
                expr_src.as_str().unwrap_or_default(),
                field,
                &cent["fields"][field],
            )?;
            if let Some(v) = v {
                data.insert(field.clone(), v);
            }
        }
        if data.get(via).and_then(Value::as_str).map(str::to_owned) == Some(rec.id.clone()) {
            return Err(rules(
                format!("/effect/{i}/carry/values/{via}"),
                "the copy must point somewhere other than the record it is carried from"
                    .to_string(),
            ));
        }
        ce_validate::validate(m, ce, &data, ce_validate::Mode::Create).map_err(|errors| {
            Stop::Problem(Problem::validation(
                "the action builds an invalid record",
                errors
                    .into_iter()
                    .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail}))
                    .collect(),
            ))
        })?;
        crate::prepare_create(m, ce, &mut data);
        // Lock where the copy is going before checking its freeze, so a close of the
        // destination either finishes first (the copy is refused) or waits for this action
        // (its snapshot then counts the copies).
        if let (Some(Value::String(dest)), Some(target)) =
            (data.get(via), cent["fields"][via]["ref"].as_str())
        {
            tx.get_locked(env.scope, target, dest)?;
        }
        let op = Op::Create {
            data,
            key: format!("act:{}:{i}:{}", r.key, src.id),
        };
        let out = writes::step(tx, env, ce, &op)?;
        writes::audit(tx, env, ce, &op, &out)?;
        if let Some(made) = out.after {
            done.created.push(json!({"entity": ce, "id": made.id}));
        }
    }
    Ok(())
}

fn run_in(tx: &dyn Store, env: &Env, r: &Run) -> Result<Done, Stop> {
    let m = env.manifest;
    let action = &m.value["actions"][r.name];
    let entity = action["entity"].as_str().unwrap_or_default();
    let ent = &m.value["entities"][entity];
    let mut rec = tx.get_locked(env.scope, entity, r.id)?;
    if rec.version != r.expected {
        return Err(StoreError::VersionConflict {
            current: rec.version,
        }
        .into());
    }
    let state_field = ent["state"]["field"].as_str();
    let state_of = |rec: &Record| -> Option<String> {
        state_field.and_then(|f| rec.data[f].as_str().map(String::from))
    };
    if let Some(req) = action["requires_state"].as_array() {
        let cur = state_of(&rec);
        if !req.iter().any(|s| s.as_str() == cur.as_deref()) {
            return Err(wrong_state(format!(
                "`{}` cannot run from state `{}`",
                r.name,
                cur.unwrap_or_default()
            )));
        }
    }
    check_input(action, r.input)?;
    writes::check_unlocked(
        tx,
        env,
        entity,
        r.id,
        rec.data.as_object().unwrap_or(&Map::new()),
    )?;

    let mut done = Done {
        record: rec.clone(),
        created: Vec::new(),
        emitted: Vec::new(),
    };
    for (i, eff) in action["effect"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(cond) = eff["when"].as_str() {
            let on = eval(
                tx,
                env,
                action,
                entity,
                &rec,
                r.input,
                cond,
                "when",
                &json!({"type": "boolean"}),
            )?;
            if on != Some(Value::Bool(true)) {
                continue;
            }
        }
        if let Some(set) = eff["set"].as_object() {
            let mut patch = Map::new();
            for (field, src) in set {
                let target = &ent["fields"][field];
                let v = eval(
                    tx,
                    env,
                    action,
                    entity,
                    &rec,
                    r.input,
                    src.as_str().unwrap_or_default(),
                    field,
                    target,
                )?;
                patch.insert(field.clone(), v.unwrap_or(Value::Null));
            }
            let op = Op::Patch {
                id: rec.id.clone(),
                expected: rec.version,
                patch,
            };
            let out = writes::step(tx, env, entity, &op)?;
            writes::audit(tx, env, entity, &op, &out)?;
            rec = out.after.ok_or(StoreError::NotFound)?;
        }
        if let Some(to) = eff["transition"].as_str() {
            let field = state_field.ok_or_else(|| {
                Stop::Problem(Problem::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "no_state_model",
                    format!("`{entity}` has no states"),
                ))
            })?;
            let from = state_of(&rec).unwrap_or_default();
            let allowed = ent["state"]["states"][from.as_str()]["to"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s == to));
            if !allowed {
                return Err(wrong_state(format!("cannot move from `{from}` to `{to}`")));
            }
            writes::check_unlocked(
                tx,
                env,
                entity,
                &rec.id,
                rec.data.as_object().unwrap_or(&Map::new()),
            )?;
            let before = rec.data.clone();
            let moved = tx.set_state(env.scope, entity, &rec.id, rec.version, field, to)?;
            let op = Op::Patch {
                id: rec.id.clone(),
                expected: rec.version,
                patch: Map::new(),
            };
            let out = Outcome {
                after: Some(moved.clone()),
                before: Some(before),
                fresh: true,
            };
            writes::audit(tx, env, entity, &op, &out)?;
            rec = moved;
        }
        if let Some(c) = eff["create"].as_object() {
            let ce = c["entity"].as_str().unwrap_or_default();
            let cent = &m.value["entities"][ce];
            let mut data = Map::new();
            for (field, src) in c["values"].as_object().into_iter().flatten() {
                let v = eval(
                    tx,
                    env,
                    action,
                    entity,
                    &rec,
                    r.input,
                    src.as_str().unwrap_or_default(),
                    field,
                    &cent["fields"][field],
                )?;
                if let Some(v) = v {
                    data.insert(field.clone(), v);
                }
            }
            ce_validate::validate(m, ce, &data, ce_validate::Mode::Create).map_err(|errors| {
                Stop::Problem(Problem::validation(
                    "the action builds an invalid record",
                    errors
                        .into_iter()
                        .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail}))
                        .collect(),
                ))
            })?;
            crate::prepare_create(m, ce, &mut data);
            let op = Op::Create {
                data,
                key: format!("act:{}:{i}", r.key),
            };
            let out = writes::step(tx, env, ce, &op)?;
            writes::audit(tx, env, ce, &op, &out)?;
            if let Some(made) = out.after {
                done.created.push(json!({"entity": ce, "id": made.id}));
            }
        }
        if let Some(c) = eff["carry"].as_object() {
            carry(tx, env, action, &rec, r, i, c, &mut done)?;
        }
        if let Some(u) = eff["update"].as_object() {
            let te = u["entity"].as_str().unwrap_or_default();
            let tent = &m.value["entities"][te];
            let id = eval(
                tx,
                env,
                action,
                entity,
                &rec,
                r.input,
                u["id"].as_str().unwrap_or_default(),
                "update.id",
                &json!({"type": "string"}),
            )?;
            let Some(Value::String(id)) = id else {
                return Err(rules(
                    format!("/effect/{i}/update/id"),
                    "the record to update is not set".to_string(),
                ));
            };
            if te == entity && id == rec.id {
                return Err(rules(
                    format!("/effect/{i}/update/id"),
                    "use `set` to change the action's own record".to_string(),
                ));
            }
            let target = tx.get_locked(env.scope, te, &id)?;
            let mut patch = Map::new();
            for (field, src) in u["set"].as_object().into_iter().flatten() {
                let v = eval(
                    tx,
                    env,
                    action,
                    entity,
                    &rec,
                    r.input,
                    src.as_str().unwrap_or_default(),
                    field,
                    &tent["fields"][field],
                )?;
                patch.insert(field.clone(), v.unwrap_or(Value::Null));
            }
            let op = Op::Patch {
                id: target.id.clone(),
                expected: target.version,
                patch,
            };
            let out = writes::step(tx, env, te, &op)?;
            writes::audit(tx, env, te, &op, &out)?;
        }
        if let Some(ev) = eff["emit"].as_str() {
            crate::events::enqueue(
                tx,
                env,
                ev,
                Some(&rec.id),
                json!({"action": r.name, "entity": entity, "id": rec.id, "actor": env.ctx.actor_id, "record": rec.data}),
            )?;
            done.emitted.push(ev.to_string());
        }
    }
    // An action that left its own record alone (only `update` effects, or every effect skipped by
    // its `when`) still moves the record on, so a retry with the same version fails with
    // `version_conflict` instead of applying the other records' changes twice.
    if rec.version == r.expected {
        rec = tx.update(env.scope, entity, &rec.id, rec.version, rec.data.clone())?;
    }
    done.record = rec;
    Ok(done)
}

pub(crate) fn run<S: Store>(store: &S, env: &Env, r: &Run) -> Result<Done, Problem> {
    let mut done = None;
    let mut stop = None;
    let res = store.atomic(&mut |tx| match run_in(tx, env, r) {
        Ok(d) => {
            done = Some(d);
            Ok(())
        }
        Err(Stop::Fail(Failure::Store(e))) => Err(e),
        Err(s) => {
            stop = Some(s);
            Err(StoreError::Aborted)
        }
    });
    match (res, stop) {
        (Ok(()), _) => done.ok_or_else(|| crate::store_problem(StoreError::Aborted)),
        (Err(_), Some(Stop::Problem(p))) => Err(p),
        (Err(_), Some(Stop::Fail(f))) => Err(crate::failure_problem(f)),
        (Err(e), None) => Err(crate::store_problem(e)),
    }
}
