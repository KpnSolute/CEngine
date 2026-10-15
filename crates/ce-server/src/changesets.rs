//! Changesets: writes to a `governed` entity by a caller who may not merge are staged as
//! operations of a changeset instead of being applied. A reviewer with the `merge_changeset`
//! action merges (every operation applies in one transaction, through the same rules and audit
//! trail as a direct write), rejects, or later reverts it.
//!
//! The feature is on when the template uses the `changesets` tool; without its entities,
//! `governed` has no runtime effect. The changeset entities are written by this module only.

use crate::problem::Problem;
use crate::writes::{self, Env, Failure, Op};
use axum::http::StatusCode;
use ce_auth::TenantScope;
use ce_manifest::Manifest;
use ce_store::{ListRequest, Record, SortDir, SortKey, Store, StoreError};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub(crate) const CHANGESET: &str = "changeset";
pub(crate) const OP: &str = "changeset_op";
/// The role action that lets a caller write governed entities directly and decide changesets.
pub(crate) const MERGE_ACTION: &str = "merge_changeset";
const MAX_OPS: usize = 200;

pub(crate) fn enabled(m: &Manifest) -> bool {
    m.value["entities"][CHANGESET].is_object() && m.value["entities"][OP].is_object()
}

pub(crate) fn is_internal(entity: &str) -> bool {
    entity == CHANGESET || entity == OP
}

/// True when writes to `entity` by callers who cannot merge are staged.
pub(crate) fn governed(m: &Manifest, entity: &str) -> bool {
    enabled(m) && !is_internal(entity) && m.value["entities"][entity]["governed"] == json!(true)
}

fn object(v: &Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn text(d: &Map<String, Value>, k: &str) -> String {
    d.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn number(d: &Map<String, Value>, k: &str) -> u64 {
    d.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn not_open() -> Problem {
    Problem::new(
        StatusCode::CONFLICT,
        "changeset_not_open",
        "the changeset is not in a state that allows this",
    )
}

/// What a staged write asks for.
pub(crate) struct Staging<'a> {
    pub actor: &'a str,
    pub entity: &'a str,
    pub operation: &'static str,
    pub record_id: Option<&'a str>,
    pub expected_version: Option<u64>,
    pub data: Option<Value>,
    /// The caller's idempotency key, for creates.
    pub key: Option<&'a str>,
    /// Add to this open changeset of the same caller instead of starting a new one.
    pub append_to: Option<&'a str>,
}

pub(crate) struct Staged {
    pub changeset_id: String,
    pub op_id: String,
}

pub(crate) fn stage<S: Store>(
    store: &S,
    scope: &TenantScope,
    s: &Staging,
) -> Result<Staged, Problem> {
    let mut out = None;
    let mut bad = None;
    let cs_key = s
        .key
        .filter(|_| s.append_to.is_none())
        .map(|k| format!("cs:{k}"));
    let op_key = s
        .key
        .filter(|_| s.append_to.is_none())
        .map(|k| format!("op:{k}"));
    let r = store.atomic(&mut |tx| {
        let (cs_id, seq) = match s.append_to {
            Some(id) => {
                let cs = tx.get_locked(scope, CHANGESET, id)?;
                let d = object(&cs.data);
                if text(&d, "status") != "proposed" || text(&d, "submitted_by") != s.actor {
                    bad = Some(not_open());
                    return Err(StoreError::Aborted);
                }
                let n = tx.child_values(scope, OP, "changeset", id, None)?.len();
                if n >= MAX_OPS {
                    bad = Some(Problem::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "changeset_full",
                        format!("a changeset holds at most {MAX_OPS} operations"),
                    ));
                    return Err(StoreError::Aborted);
                }
                (id.to_string(), n as u64 + 1)
            }
            None => {
                let title = format!("{} {}", s.operation, s.entity);
                let rec = tx.insert(
                    scope,
                    CHANGESET,
                    json!({"title": title, "status": "proposed", "submitted_by": s.actor}),
                    cs_key.as_deref(),
                )?;
                (rec.id, 1)
            }
        };
        let mut op = Map::new();
        op.insert("changeset".into(), json!(cs_id));
        op.insert("seq".into(), json!(seq));
        op.insert("entity".into(), json!(s.entity));
        op.insert("operation".into(), json!(s.operation));
        if let Some(id) = s.record_id {
            op.insert("record_id".into(), json!(id));
        }
        if let Some(v) = s.expected_version {
            op.insert("expected_version".into(), json!(v));
        }
        if let Some(d) = &s.data {
            op.insert("data".into(), d.clone());
        }
        let rec = tx.insert(scope, OP, Value::Object(op), op_key.as_deref())?;
        out = Some(Staged {
            changeset_id: cs_id,
            op_id: rec.id,
        });
        Ok(())
    });
    match r {
        Ok(()) => out.ok_or_else(|| crate::store_problem(StoreError::Aborted)),
        Err(e) => Err(bad.unwrap_or_else(|| crate::store_problem(e))),
    }
}

fn ops(tx: &dyn Store, scope: &TenantScope, cs: &str) -> Result<Vec<Record>, StoreError> {
    let req = ListRequest {
        select: None,
        filter: Some(json!({"field": "changeset", "op": "eq", "value": cs})),
        search: None,
        sort: vec![SortKey {
            field: "seq".into(),
            dir: SortDir::Asc,
        }],
        limit: Some(MAX_OPS),
        after: None,
    };
    let page = tx.list(scope, OP, &req)?;
    Ok(page.records)
}

/// Failures that mean the changeset no longer applies to the data (as against an outage).
fn is_conflict(f: &Failure) -> bool {
    matches!(
        f,
        Failure::Rules(_)
            | Failure::Locked
            | Failure::Store(
                StoreError::VersionConflict { .. }
                    | StoreError::NotFound
                    | StoreError::Constraint(_)
            )
    )
}

fn set_status(
    tx: &dyn Store,
    scope: &TenantScope,
    cs: &Record,
    status: &str,
    by: &str,
    conflict: Option<Value>,
) -> Result<Record, StoreError> {
    let mut d = object(&cs.data);
    d.insert("status".into(), json!(status));
    d.insert("decided_by".into(), json!(by));
    if let Some(c) = conflict {
        d.insert("conflict".into(), c);
    }
    tx.update(scope, CHANGESET, &cs.id, cs.version, Value::Object(d))
}

/// Runs the operations of `id` (merge) or undoes them (revert) in one transaction.
fn apply<S: Store>(
    store: &S,
    env: &Env,
    id: &str,
    revert: bool,
    mut preview: Option<&mut Preview>,
) -> Result<Option<Record>, Problem> {
    let mut done = None;
    let mut bad = None;
    let mut failed: Option<(u64, Failure)> = None;
    let r = store.atomic(&mut |tx| {
        let cs = tx.get_locked(env.scope, CHANGESET, id)?;
        let want = if revert { "merged" } else { "proposed" };
        if text(&object(&cs.data), "status") != want {
            bad = Some(not_open());
            return Err(StoreError::Aborted);
        }
        // Four eyes: the person who staged the work does not approve it, even holding both roles.
        if !revert
            && preview.is_none()
            && text(&object(&cs.data), "submitted_by") == env.ctx.actor_id
        {
            bad = Some(Problem::new(
                StatusCode::FORBIDDEN,
                "self_merge",
                "a changeset cannot be merged by the person who submitted it",
            ));
            return Err(StoreError::Aborted);
        }
        let mut list = ops(tx, env.scope, id)?;
        if revert {
            list.reverse();
        }
        // The version each record has reached inside this changeset, so several operations on
        // one record chain instead of conflicting with each other.
        let mut versions: HashMap<(String, String), u64> = HashMap::new();
        for rec in list {
            let d = object(&rec.data);
            let (entity, seq) = (text(&d, "entity"), number(&d, "seq"));
            let operation = text(&d, "operation");
            let mut rid = text(&d, "record_id");
            let patch = object(d.get("data").unwrap_or(&Value::Null));
            let chosen_id = patch.contains_key("id");
            let known = versions.get(&(entity.clone(), rid.clone())).copied();
            let wop = if revert {
                let before = object(d.get("before").unwrap_or(&Value::Null));
                let at = known.unwrap_or_else(|| number(&d, "result_version"));
                match operation.as_str() {
                    "create" => Op::Delete {
                        id: rid.clone(),
                        expected: at,
                    },
                    "update" => Op::Patch {
                        id: rid.clone(),
                        expected: at,
                        patch: patch
                            .keys()
                            .map(|k| (k.clone(), before.get(k).cloned().unwrap_or(Value::Null)))
                            .collect(),
                    },
                    _ => {
                        bad = Some(Problem::new(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "revert_unsupported",
                            "a changeset that deletes records cannot be reverted",
                        ));
                        return Err(StoreError::Aborted);
                    }
                }
            } else {
                let expected = known.unwrap_or_else(|| number(&d, "expected_version"));
                match operation.as_str() {
                    "create" => Op::Create {
                        data: patch,
                        key: format!("cs-{id}-{seq}"),
                    },
                    "update" => Op::Patch {
                        id: rid.clone(),
                        expected,
                        patch,
                    },
                    _ => Op::Delete {
                        id: rid.clone(),
                        expected,
                    },
                }
            };
            let out = match writes::step(tx, env, &entity, &wop)
                .and_then(|o| writes::audit(tx, env, &entity, &wop, &o).map(|()| o))
            {
                Ok(o) => o,
                Err(f) => {
                    // A preview reports what would stop the merge (rules, locks, a record that
                    // changed). An outage is not a verdict on the changeset and stays an error.
                    if let (true, Some(p)) = (is_conflict(&f), preview.as_deref_mut()) {
                        let pr = crate::failure_problem(f);
                        p.failure = Some(json!({
                            "seq": seq, "entity": entity, "operation": operation,
                            "code": pr.code, "status": pr.status.as_u16(),
                            "detail": pr.detail, "errors": pr.errors,
                        }));
                        break;
                    }
                    failed = Some((seq, f));
                    return Err(StoreError::Aborted);
                }
            };
            if let Some(p) = preview.as_deref_mut() {
                p.results.push(json!({
                    "seq": seq, "entity": entity, "operation": operation,
                    // A create without a chosen id gets one from the store; a rolled-back
                    // preview cannot promise the merge will pick the same.
                    "id_provisional": operation == "create" && rid.is_empty() && !chosen_id,
                    "record": out.after.as_ref().map(|r| json!({
                        "id": r.id, "version": r.version, "data": r.data,
                    })),
                }));
            }
            if let Some(after) = &out.after {
                if matches!(wop, Op::Create { .. }) {
                    rid = after.id.clone();
                }
                versions.insert((entity.clone(), rid.clone()), after.version);
            }
            if !revert && preview.is_none() {
                let mut nd = d.clone();
                nd.insert("record_id".into(), json!(rid));
                if let Some(b) = &out.before {
                    nd.insert("before".into(), b.clone());
                }
                if let Some(a) = &out.after {
                    nd.insert("result_version".into(), json!(a.version));
                } else {
                    nd.remove("result_version");
                }
                tx.update(env.scope, OP, &rec.id, rec.version, Value::Object(nd))?;
            }
        }
        if let Some(p) = preview.as_deref_mut() {
            // Later operations can change an earlier record (a parent's totals follow its
            // children), so each is read again as the transaction now holds it.
            for r in &mut p.results {
                let (entity, rid) = (
                    text(&object(r), "entity"),
                    text(&object(&r["record"]), "id"),
                );
                if rid.is_empty() {
                    continue;
                }
                match tx.get(env.scope, &entity, &rid) {
                    Ok(now) => {
                        r["record"] =
                            json!({"id": now.id, "version": now.version, "data": now.data});
                    }
                    // A later operation deleted it: the merge leaves nothing behind.
                    Err(StoreError::NotFound) => r["record"] = Value::Null,
                    Err(e) => return Err(e),
                }
                if r["id_provisional"] == true && r["record"].is_object() {
                    r["record"]["id"] = Value::Null;
                }
            }
        }
        if preview.is_some() {
            // Nothing is kept: the transaction ends here with everything it did undone.
            return Err(StoreError::Aborted);
        }
        let status = if revert { "reverted" } else { "merged" };
        let finished = set_status(tx, env.scope, &cs, status, &env.ctx.actor_id, None)?;
        // With the `events` tool the outcome is an event, stored with the merge: subscriptions
        // and relays see exactly the merges that happened.
        crate::events::enqueue(
            tx,
            env,
            if revert {
                "changeset.reverted"
            } else {
                "changeset.merged"
            },
            Some(&finished.id),
            json!({"id": finished.id, "actor": env.ctx.actor_id, "title": finished.data["title"]}),
        )?;
        done = Some(finished);
        Ok(())
    });
    match r {
        Ok(()) => done
            .map(Some)
            .ok_or_else(|| crate::store_problem(StoreError::Aborted)),
        Err(e) => {
            if let Some(p) = bad {
                return Err(p);
            }
            if preview.is_some() && matches!(e, StoreError::Aborted) {
                return Ok(None);
            }
            let Some((seq, f)) = failed else {
                return Err(crate::store_problem(e));
            };
            // A merge that no longer applies is recorded; a failed revert leaves the changeset merged.
            if !revert && is_conflict(&f) {
                let detail = json!({"seq": seq, "reason": failure_problem_code(&f)});
                let _ = store.atomic(&mut |tx| {
                    let cs = tx.get_locked(env.scope, CHANGESET, id)?;
                    if text(&object(&cs.data), "status") == "proposed" {
                        set_status(
                            tx,
                            env.scope,
                            &cs,
                            "conflicted",
                            &env.ctx.actor_id,
                            Some(detail.clone()),
                        )?;
                    }
                    Ok(())
                });
            }
            Err(crate::failure_problem(f))
        }
    }
}

fn failure_problem_code(f: &Failure) -> &'static str {
    match f {
        Failure::Rules(_) => "rule_broken",
        Failure::Locked => "entity_locked",
        Failure::Store(StoreError::VersionConflict { .. }) => "version_conflict",
        Failure::Store(StoreError::NotFound) => "not_found",
        Failure::Store(_) => "store",
    }
}

pub(crate) fn merge<S: Store>(store: &S, env: &Env, id: &str) -> Result<Record, Problem> {
    apply(store, env, id, false, None)?.ok_or_else(|| crate::store_problem(StoreError::Aborted))
}

pub(crate) fn revert<S: Store>(store: &S, env: &Env, id: &str) -> Result<Record, Problem> {
    apply(store, env, id, true, None)?.ok_or_else(|| crate::store_problem(StoreError::Aborted))
}

/// What a merge would do, without doing it: every operation applied in one transaction that is
/// always rolled back. `results` has each operation's record as the merge would store it (with
/// computed fields), so a reviewer sees totals and reconciliation before deciding. The first
/// operation that would fail is reported as `failure` and the ones after it are not run.
pub(crate) fn preview<S: Store>(store: &S, env: &Env, id: &str) -> Result<Value, Problem> {
    let mut out = Preview::default();
    apply(store, env, id, false, Some(&mut out))?;
    Ok(json!({
        "mergeable": out.failure.is_none(),
        "results": out.results,
        "failure": out.failure,
    }))
}

#[derive(Default)]
struct Preview {
    results: Vec<Value>,
    failure: Option<Value>,
}

/// `reject` (a reviewer) or `withdraw` (the submitter): a proposed changeset becomes closed.
pub(crate) fn close<S: Store>(
    store: &S,
    scope: &TenantScope,
    actor: &str,
    id: &str,
    status: &str,
    submitter_only: bool,
) -> Result<Record, Problem> {
    let mut done = None;
    let mut bad = None;
    let r = store.atomic(&mut |tx| {
        let cs = tx.get_locked(scope, CHANGESET, id)?;
        let d = object(&cs.data);
        if text(&d, "status") != "proposed" || (submitter_only && text(&d, "submitted_by") != actor)
        {
            bad = Some(not_open());
            return Err(StoreError::Aborted);
        }
        done = Some(set_status(tx, scope, &cs, status, actor, None)?);
        Ok(())
    });
    match r {
        Ok(()) => done.ok_or_else(|| crate::store_problem(StoreError::Aborted)),
        Err(e) => Err(bad.unwrap_or_else(|| crate::store_problem(e))),
    }
}
