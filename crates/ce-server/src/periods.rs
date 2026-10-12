//! Periods: closing one freezes the records that point at it, through the `locked_when` rule
//! the template puts on those entities (for example `ref(period).status == 'closed'`).
//! The period's `status`, `closed_by`, `closed_at` and `snapshot` are set by this module only.
//! A close records how many live records of each entity point at the period.

use crate::writes::{self, Env, Op, Outcome};
use axum::http::StatusCode;
use ce_manifest::Manifest;
use ce_store::{Store, StoreError};
use serde_json::{json, Map, Value};

use crate::{Problem, Record};

pub(crate) const PERIOD: &str = "period";
/// The role action that lets a caller close and reopen periods.
pub(crate) const ACTION: &str = "close_period";
const SERVER_FIELDS: [&str; 4] = ["status", "closed_by", "closed_at", "snapshot"];

pub(crate) fn enabled(m: &Manifest) -> bool {
    m.value["entities"][PERIOD].is_object() && m.value["actions"][ACTION].is_object()
}

/// A request body may not set what only a close or reopen sets.
pub(crate) fn touches_server_fields(op: &Op) -> bool {
    match op {
        Op::Create { data, .. } => {
            data.get("status").is_some_and(|s| s != "open")
                || SERVER_FIELDS[1..].iter().any(|k| data.contains_key(*k))
        }
        Op::Patch { patch, .. } => SERVER_FIELDS.iter().any(|k| patch.contains_key(*k)),
        Op::Delete { .. } => false,
    }
}

fn snapshot(tx: &dyn Store, env: &Env, id: &str) -> Result<Value, StoreError> {
    let mut counts = Map::new();
    let ents = env.manifest.value["entities"].as_object();
    for (name, ent) in ents.into_iter().flatten() {
        for (fname, f) in ent["fields"].as_object().into_iter().flatten() {
            if f["type"] == "ref" && f["ref"] == PERIOD && name != PERIOD {
                let n = tx.child_values(env.scope, name, fname, id, None)?.len();
                counts.insert(name.clone(), json!(n));
            }
        }
    }
    Ok(json!({"counts": counts, "at": env.ctx.now_unix}))
}

fn refuse(detail: &str) -> Problem {
    Problem::new(StatusCode::CONFLICT, "invalid_transition", detail)
}

/// Closes (`close`) or reopens a period in one transaction, with its audit row.
pub(crate) fn transition<S: Store>(
    store: &S,
    env: &Env,
    id: &str,
    close: bool,
) -> Result<Record, Problem> {
    let mut done = None;
    let mut bad = None;
    let r = store.atomic(&mut |tx| {
        let cur = tx.get_locked(env.scope, PERIOD, id)?;
        let status = cur.data["status"].as_str().unwrap_or("open");
        if (status == "closed") == close {
            bad = Some(refuse(if close {
                "the period is already closed"
            } else {
                "the period is not closed"
            }));
            return Err(StoreError::Aborted);
        }
        let mut patch = Map::new();
        if close {
            patch.insert("status".into(), json!("closed"));
            patch.insert("closed_by".into(), json!(env.ctx.actor_id));
            patch.insert("closed_at".into(), json!(env.ctx.now_unix));
            patch.insert("snapshot".into(), snapshot(tx, env, id)?);
        } else {
            patch.insert("status".into(), json!("open"));
            patch.insert("closed_by".into(), Value::Null);
            patch.insert("closed_at".into(), Value::Null);
        }
        let op = Op::Patch {
            id: id.to_string(),
            expected: cur.version,
            patch,
        };
        let out: Outcome = match writes::step(tx, env, PERIOD, &op)
            .and_then(|o| writes::audit(tx, env, PERIOD, &op, &o).map(|()| o))
        {
            Ok(o) => o,
            Err(writes::Failure::Store(e)) => return Err(e),
            Err(_) => return Err(StoreError::Aborted),
        };
        if let Some(p) = &out.after {
            let name = if close {
                "period.closed"
            } else {
                "period.reopened"
            };
            crate::events::enqueue(
                tx,
                env,
                name,
                Some(&p.id),
                json!({"id": p.id, "actor": env.ctx.actor_id, "period": p.data}),
            )?;
        }
        done = out.after;
        Ok(())
    });
    match r {
        Ok(()) => done.ok_or_else(|| crate::store_problem(StoreError::Aborted)),
        Err(e) => Err(bad.unwrap_or_else(|| crate::store_problem(e))),
    }
}
