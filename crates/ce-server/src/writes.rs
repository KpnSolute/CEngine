//! Writes that keep derived records consistent. A create, patch or delete runs in one store
//! transaction together with every parent whose totals depend on the record.
//!
//! A parent depends on a child when one of its computed fields reads `children(child, field)`.
//! After the child is written, each affected parent is locked, its computed fields are
//! recomputed from the children as they now stand, its invariants are checked, and it is
//! saved if anything changed. That save can in turn change a grandparent, so the walk goes up
//! (at most [`MAX_DEPTH`] levels). If any rule fails, nothing is kept. Parents are locked in id
//! order so two writers cannot wait on each other.
//!
//! `ref()` is read when the record itself is written; it is not recomputed when the referenced
//! record changes later.

use ce_auth::TenantScope;
use ce_expr::{Resolver, Value as Ev};
use ce_manifest::Manifest;
use ce_store::{Record, Store, StoreError};
use ce_validate::{
    aggregate_sources, check_invariants_with, evaluate_computed_with, json_to_expr, Context,
    FieldError,
};
use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

/// How many levels of parents a write may update (child, parent, grandparent...).
pub(crate) const MAX_DEPTH: usize = 8;

pub(crate) enum Op {
    Create {
        data: Map<String, Value>,
        key: String,
    },
    Patch {
        id: String,
        expected: u64,
        patch: Map<String, Value>,
    },
    Delete {
        id: String,
        expected: u64,
    },
}

pub(crate) enum Failure {
    Store(StoreError),
    Rules(Vec<FieldError>),
    /// The record sits in a frozen window (`locked_when` holds): it cannot change.
    Locked,
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Failure::Store(e)
    }
}

pub(crate) struct Env<'a> {
    pub manifest: &'a Manifest,
    pub scope: &'a TenantScope,
    pub ctx: &'a Context,
    /// Id of the request that causes the write, recorded in the audit trail.
    pub request_id: &'a str,
}

/// Answers `ref()` and `children()` from the store, for one record of one tenant.
struct StoreResolver<'a> {
    store: &'a dyn Store,
    env: &'a Env<'a>,
    entity: &'a str,
    /// None while the record is being created: it has no children yet.
    id: Option<&'a str>,
    /// The first store failure met while answering. The evaluator only sees a message, so this
    /// keeps the real error: a backend or unsupported-store failure is not a broken rule.
    failed: RefCell<Option<StoreError>>,
    /// What was read from the store while settling this record, so a record read twice gives
    /// the same answer both times (Postgres reads committed data afresh on every statement).
    seen: RefCell<HashMap<String, Option<Value>>>,
}

impl StoreResolver<'_> {
    fn fail(&self, e: StoreError) -> String {
        let msg = format!("{e:?}");
        self.failed.borrow_mut().get_or_insert(e);
        msg
    }
}

fn field_type<'m>(m: &'m Manifest, entity: &str, field: &str) -> &'m str {
    m.value["entities"][entity]["fields"][field]["type"]
        .as_str()
        .unwrap_or("")
}

impl Resolver for StoreResolver<'_> {
    fn ref_attr(&self, field: &str, id: &str, attr: &str) -> Result<Ev, String> {
        let m = self.env.manifest;
        let target = m.value["entities"][self.entity]["fields"][field]["ref"]
            .as_str()
            .ok_or("not a ref field")?;
        let key = format!("ref\u{0}{target}\u{0}{id}");
        let cached = self.seen.borrow().get(&key).cloned();
        let data = match cached {
            Some(d) => d,
            None => {
                let d = match self.store.get(self.env.scope, target, id) {
                    Ok(rec) => Some(rec.data),
                    Err(StoreError::NotFound) => None,
                    Err(e) => return Err(self.fail(e)),
                };
                self.seen.borrow_mut().insert(key, d.clone());
                d
            }
        };
        Ok(data
            .as_ref()
            .and_then(|d| d.get(attr))
            .map_or(Ev::Null, |v| json_to_expr(field_type(m, target, attr), v)))
    }

    fn children(
        &self,
        entity: &str,
        ref_field: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Ev>, String> {
        let Some(id) = self.id else {
            return Ok(Vec::new());
        };
        let key = format!(
            "kids\u{0}{entity}\u{0}{ref_field}\u{0}{}",
            attr.unwrap_or("")
        );
        let cached = self.seen.borrow().get(&key).cloned().flatten();
        let values: Vec<Value> = match cached {
            Some(Value::Array(v)) => v,
            _ => {
                let v = self
                    .store
                    .child_values(self.env.scope, entity, ref_field, id, attr)
                    .map_err(|e| self.fail(e))?;
                self.seen
                    .borrow_mut()
                    .insert(key, Some(Value::Array(v.clone())));
                v
            }
        };
        let ty = attr.map_or("", |a| field_type(self.env.manifest, entity, a));
        Ok(values.iter().map(|v| json_to_expr(ty, v)).collect())
    }
}

/// Computes the record's derived fields and checks its rules, against the store as it is now.
fn settle(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    id: Option<&str>,
    record: &mut Map<String, Value>,
    old: Option<&Map<String, Value>>,
) -> Result<(), Failure> {
    let r = StoreResolver {
        store: tx,
        env,
        entity,
        id,
        failed: RefCell::new(None),
        seen: RefCell::new(HashMap::new()),
    };
    let out = evaluate_computed_with(env.manifest, entity, record, old, env.ctx, Some(&r))
        .and_then(|()| check_invariants_with(env.manifest, entity, record, old, env.ctx, Some(&r)));
    match (out, r.failed.into_inner()) {
        (_, Some(e)) => Err(Failure::Store(e)),
        (Err(errors), None) => Err(Failure::Rules(errors)),
        (Ok(()), None) => Ok(()),
    }
}

/// Runs `f` with a resolver for the record `id` of `entity` (None while it does not exist yet),
/// and tells a store failure met while resolving from a rule that did not hold.
pub(crate) fn with_resolver<T>(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    id: Option<&str>,
    f: impl FnOnce(&dyn Resolver) -> T,
) -> Result<T, Failure> {
    let r = StoreResolver {
        store: tx,
        env,
        entity,
        id,
        failed: RefCell::new(None),
        seen: RefCell::new(HashMap::new()),
    };
    let out = f(&r);
    match r.failed.into_inner() {
        Some(e) => Err(Failure::Store(e)),
        None => Ok(out),
    }
}

/// Refuses a move or write on a record frozen by `locked_when`.
pub(crate) fn check_unlocked(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    id: &str,
    record: &Map<String, Value>,
) -> Result<(), Failure> {
    guard_locked(tx, env, entity, Some(id), record, None)
}

/// Refuses a record being put into a frozen window: when the write sets a `ref` field (the way a
/// record joins a period), the record as it would become must not be locked. A write that only
/// changes plain fields may lock its own record (a status that freezes it), so the result of
/// those is not held against it; the stored record was already checked.
fn guard_moved<'k>(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    id: Option<&str>,
    record: &Map<String, Value>,
    old: Option<&Map<String, Value>>,
    changed: impl Iterator<Item = &'k String>,
) -> Result<(), Failure> {
    let fields = &env.manifest.value["entities"][entity]["fields"];
    let mut changed = changed;
    if changed.any(|k| fields[k.as_str()]["type"] == "ref") {
        guard_locked(tx, env, entity, id, record, old)?;
    }
    Ok(())
}

/// Refuses the write when `locked_when` holds for the record, as stored or as it would become.
/// A period close freezes the records that point at the period this way.
fn guard_locked(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    id: Option<&str>,
    record: &Map<String, Value>,
    old: Option<&Map<String, Value>>,
) -> Result<(), Failure> {
    if env.manifest.value["entities"][entity]["locked_when"].is_null() {
        return Ok(());
    }
    let r = StoreResolver {
        store: tx,
        env,
        entity,
        id,
        failed: RefCell::new(None),
        seen: RefCell::new(HashMap::new()),
    };
    let locked = ce_validate::is_locked(env.manifest, entity, record, old, env.ctx, &r);
    match r.failed.into_inner() {
        Some(e) => Err(Failure::Store(e)),
        None if locked => Err(Failure::Locked),
        None => Ok(()),
    }
}

/// The `(entity, id)` of every parent whose totals a write to `child` changes.
fn parent_keys(
    env: &Env,
    child: &str,
    old: Option<&Map<String, Value>>,
    new: Option<&Map<String, Value>>,
) -> BTreeSet<(String, String)> {
    let mut parents = BTreeSet::new();
    for src in aggregate_sources(env.manifest)
        .into_iter()
        .filter(|s| s.child == child)
    {
        for d in [old, new].into_iter().flatten() {
            if let Some(id) = d.get(&src.ref_field).and_then(Value::as_str) {
                parents.insert((src.parent.clone(), id.to_string()));
            }
        }
    }
    parents
}

/// Locks, before a write touches anything, every record it will change: its own row (`own`, for
/// a patch or delete) and everything above it. All writers sort what they lock by (entity, id)
/// and take the locks in that one global order, so they cannot wait on each other in a circle:
/// a child writer that goes on to update its parent and grandparent, and a parent delete that
/// holds the parent and waits for its children, would otherwise each lock in their own order.
/// (Writing a child row also takes a share lock on its parent, so locking the parent after the
/// row is the same hazard.)
fn lock_parents(
    tx: &dyn Store,
    env: &Env,
    child: &str,
    own: Option<&str>,
    old: Option<&Map<String, Value>>,
    new: Option<&Map<String, Value>>,
) -> Result<(), Failure> {
    // A parent can move while its links are read without a lock, so after locking, the walk is
    // repeated; if it found more records the new ones are locked too (a few tries at most).
    let mut held: BTreeSet<(String, String)> = BTreeSet::new();
    for _ in 0..3 {
        let mut want: BTreeSet<(String, String)> = ancestor_levels(tx, env, child, old, new)?
            .into_iter()
            .flatten()
            .collect();
        if let Some(id) = own {
            want.insert((child.to_string(), id.to_string()));
        }
        let mut locked_any = false;
        for key in want {
            if held.insert(key.clone()) {
                locked_any = true;
                match tx.get_locked(env.scope, &key.0, &key.1) {
                    Ok(_) | Err(StoreError::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if !locked_any {
            break;
        }
    }
    Ok(())
}

/// The records above a write, nearest first: level 0 are its parents, level 1 theirs, and so on.
fn ancestor_levels(
    tx: &dyn Store,
    env: &Env,
    child: &str,
    old: Option<&Map<String, Value>>,
    new: Option<&Map<String, Value>>,
) -> Result<Vec<BTreeSet<(String, String)>>, Failure> {
    let mut levels = Vec::new();
    let mut frontier = parent_keys(env, child, old, new);
    while !frontier.is_empty() && levels.len() < MAX_DEPTH {
        let mut next = BTreeSet::new();
        for (entity, id) in &frontier {
            match tx.get(env.scope, entity, id) {
                Ok(r) => next.extend(parent_keys(env, entity, r.data.as_object(), None)),
                // A dangling reference has no ancestors.
                Err(StoreError::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        levels.push(std::mem::take(&mut frontier));
        frontier = next;
    }
    Ok(levels)
}

type Change = (
    String,
    Option<Map<String, Value>>,
    Option<Map<String, Value>>,
);

/// Brings every parent of a changed child up to date. `old` and `new` are the child's data
/// before and after the write (`None` when it did not exist).
fn refresh_parents(
    tx: &dyn Store,
    env: &Env,
    child: &str,
    old: Option<&Map<String, Value>>,
    new: Option<&Map<String, Value>>,
) -> Result<(), Failure> {
    let mut changes: Vec<Change> = vec![(child.to_string(), old.cloned(), new.cloned())];
    // One level at a time: every parent that any change of the level touches is locked, then
    // recomputed once, before the next level is looked at. A rule of a higher level so sees all
    // of the level's new totals together, and a record reached by two branches changes once.
    for depth in 0.. {
        let mut parents: BTreeSet<(String, String)> = BTreeSet::new();
        for (entity, old, new) in &changes {
            parents.extend(parent_keys(env, entity, old.as_ref(), new.as_ref()));
        }
        if parents.is_empty() {
            return Ok(());
        }
        if depth >= MAX_DEPTH {
            return Err(Failure::Store(StoreError::Constraint("aggregate depth")));
        }
        let mut locked = Vec::new();
        for (parent, id) in parents {
            match tx.get_locked(env.scope, &parent, &id) {
                Ok(r) => locked.push((parent, id, r)),
                // A dangling reference has no total to keep.
                Err(StoreError::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        changes = Vec::new();
        for (parent, id, current) in locked {
            let Value::Object(before) = &current.data else {
                continue;
            };
            let mut after = before.clone();
            settle(tx, env, &parent, Some(&id), &mut after, Some(before))?;
            if &after != before {
                tx.update(
                    env.scope,
                    &parent,
                    &id,
                    current.version,
                    Value::Object(after.clone()),
                )?;
                changes.push((parent, Some(before.clone()), Some(after)));
            }
        }
    }
    Ok(())
}

/// What a write changed: the record it left (none for a delete) and the data it replaced
/// (none for a create), which is what the audit trail keeps.
pub(crate) struct Outcome {
    pub(crate) after: Option<Record>,
    pub(crate) before: Option<Value>,
    /// False when a create was a replay of an earlier request with the same key: nothing
    /// changed, so there is nothing to audit.
    pub(crate) fresh: bool,
}

pub(crate) fn step(tx: &dyn Store, env: &Env, entity: &str, op: &Op) -> Result<Outcome, Failure> {
    match op {
        Op::Create { data, key } => {
            // The derived fields are computed only if this call really creates the record: a
            // retry of the same request replays the first result even when the records they
            // read have changed since (or would now break a rule).
            lock_parents(tx, env, entity, None, None, Some(data))?;
            let mut failed = None;
            let mut derived = false;
            let rec = tx.insert_with(
                env.scope,
                entity,
                Value::Object(data.clone()),
                &mut || {
                    derived = true;
                    let mut obj = data.clone();
                    match settle(tx, env, entity, None, &mut obj, None)
                        .and_then(|()| guard_moved(tx, env, entity, None, &obj, None, data.keys()))
                    {
                        Ok(()) => {
                            obj.retain(|k, v| data.get(k) != Some(&*v));
                            Ok(obj)
                        }
                        Err(f) => {
                            let e = match &f {
                                Failure::Store(e) => e.clone(),
                                Failure::Rules(_) | Failure::Locked => StoreError::Aborted,
                            };
                            failed = Some(f);
                            Err(e)
                        }
                    }
                },
                Some(key),
            );
            let rec = match (rec, failed) {
                (_, Some(f)) => return Err(f),
                (Ok(r), None) => r,
                (Err(e), None) => return Err(e.into()),
            };
            // A replayed create changed nothing, so the parents are already right.
            if derived {
                refresh_parents(tx, env, entity, None, rec.data.as_object())?;
            }
            Ok(Outcome {
                after: Some(rec),
                before: None,
                fresh: derived,
            })
        }
        Op::Patch {
            id,
            expected,
            patch,
        } => {
            let pre = tx.get(env.scope, entity, id)?;
            let mut moved = pre.data.clone();
            crate::merge_patch(&mut moved, &Value::Object(patch.clone()));
            lock_parents(
                tx,
                env,
                entity,
                Some(id),
                pre.data.as_object(),
                moved.as_object(),
            )?;
            let current = tx.get_locked(env.scope, entity, id)?;
            if current.version != *expected {
                return Err(StoreError::VersionConflict {
                    current: current.version,
                }
                .into());
            }
            let mut data = current.data.clone();
            crate::merge_patch(&mut data, &Value::Object(patch.clone()));
            let Value::Object(mut obj) = data else {
                return Err(StoreError::BadData("data must be a JSON object").into());
            };
            let old = current.data.as_object();
            guard_locked(
                tx,
                env,
                entity,
                Some(id),
                current.data.as_object().unwrap_or(&Map::new()),
                None,
            )?;
            settle(tx, env, entity, Some(id), &mut obj, old)?;
            guard_moved(tx, env, entity, Some(id), &obj, old, patch.keys())?;
            let rec = tx.update(env.scope, entity, id, *expected, Value::Object(obj))?;
            refresh_parents(tx, env, entity, old, rec.data.as_object())?;
            Ok(Outcome {
                after: Some(rec),
                before: Some(current.data.clone()),
                fresh: true,
            })
        }
        Op::Delete { id, expected } => {
            let pre = tx.get(env.scope, entity, id)?;
            lock_parents(tx, env, entity, Some(id), pre.data.as_object(), None)?;
            let current = tx.get_locked(env.scope, entity, id)?;
            guard_locked(
                tx,
                env,
                entity,
                Some(id),
                current.data.as_object().unwrap_or(&Map::new()),
                None,
            )?;
            tx.delete(env.scope, entity, id, *expected)?;
            refresh_parents(tx, env, entity, current.data.as_object(), None)?;
            Ok(Outcome {
                after: None,
                before: Some(current.data.clone()),
                fresh: true,
            })
        }
    }
}

/// The entity the `audit` tool adds. Only the server writes it.
pub(crate) const AUDIT_ENTITY: &str = "audit_event";

/// Records the write in the audit trail, in the transaction of the write itself, when the
/// template uses the `audit` tool. The audit entity is never audited.
pub(crate) fn audit(
    tx: &dyn Store,
    env: &Env,
    entity: &str,
    op: &Op,
    out: &Outcome,
) -> Result<(), Failure> {
    if entity == AUDIT_ENTITY
        || !out.fresh
        || !env.manifest.value["entities"][AUDIT_ENTITY].is_object()
    {
        return Ok(());
    }
    let (after, before) = (&out.after, &out.before);
    let (operation, id) = match (op, after) {
        (Op::Create { .. }, Some(r)) => ("create", r.id.clone()),
        (Op::Patch { id, .. }, _) => ("update", id.clone()),
        (Op::Delete { id, .. }, _) => ("delete", id.clone()),
        (Op::Create { .. }, None) => return Ok(()),
    };
    let mut ev = Map::new();
    ev.insert("entity".into(), json!(entity));
    ev.insert("record_id".into(), json!(id));
    ev.insert("operation".into(), json!(operation));
    ev.insert("actor".into(), json!(env.ctx.actor_id));
    ev.insert("request_id".into(), json!(env.request_id));
    ev.insert("at".into(), json!(env.ctx.now_unix));
    if let Some(r) = after {
        ev.insert("to_version".into(), json!(r.version));
        ev.insert("after".into(), r.data.clone());
    }
    if let Some(b) = before {
        ev.insert("before".into(), b.clone());
    }
    tx.insert(env.scope, AUDIT_ENTITY, Value::Object(ev), None)?;
    Ok(())
}

/// Runs several writes in one transaction, in order: all are kept or none. Each result is the
/// record after the write (None for a delete). An operation sees the changes of the ones before it.
pub(crate) fn execute_all<S: Store>(
    store: &S,
    env: &Env,
    ops: &[(String, Op)],
) -> Result<Vec<Option<Record>>, Failure> {
    execute_all_with(store, env, ops, false)
}

/// Like [`execute_all`]; with `rehearse` every rule runs but the transaction is rolled back at the
/// end, so a success keeps nothing.
pub(crate) fn execute_all_with<S: Store>(
    store: &S,
    env: &Env,
    ops: &[(String, Op)],
    rehearse: bool,
) -> Result<Vec<Option<Record>>, Failure> {
    let mut done = Vec::new();
    let mut failed = None;
    let out = store.atomic(&mut |tx| {
        done.clear();
        // Lock the records the batch updates or deletes in one global (entity, id) order before
        // running the operations in the caller's order, so two batches that touch the same
        // records in opposite orders take turns instead of waiting on each other in a circle.
        // (A missing record is left for its own operation to report.)
        let mut own: BTreeSet<(&str, &str)> = BTreeSet::new();
        for (entity, op) in ops {
            if let Op::Patch { id, .. } | Op::Delete { id, .. } = op {
                own.insert((entity.as_str(), id.as_str()));
            }
        }
        for (entity, id) in own {
            match tx.get_locked(env.scope, entity, id) {
                Ok(_) | Err(StoreError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        for (entity, op) in ops {
            match step(tx, env, entity, op).and_then(|o| audit(tx, env, entity, op, &o).map(|()| o))
            {
                Ok(o) => done.push(o.after),
                Err(f) => {
                    let e = match &f {
                        Failure::Store(e) => e.clone(),
                        Failure::Rules(_) | Failure::Locked => StoreError::Aborted,
                    };
                    failed = Some(f);
                    return Err(e);
                }
            }
        }
        if rehearse {
            return Err(StoreError::Aborted);
        }
        Ok(())
    });
    match out {
        Ok(()) => Ok(done),
        Err(StoreError::Aborted) if rehearse && failed.is_none() => Ok(done),
        Err(e) => Err(failed.unwrap_or(Failure::Store(e))),
    }
}

/// Runs one write and everything it changes as a single transaction. Stores without
/// transactions can still do writes that touch no totals; a write that would need to update a
/// parent is refused there instead of risking a total that disagrees with its children.
pub(crate) fn execute<S: Store>(
    store: &S,
    env: &Env,
    entity: &str,
    op: &Op,
) -> Result<Option<Record>, Failure> {
    let mut done = None;
    let mut failed = None;
    let out = store.atomic(&mut |tx| match step(tx, env, entity, op)
        .and_then(|o| audit(tx, env, entity, op, &o).map(|()| o))
    {
        Ok(o) => {
            done = Some(o.after);
            Ok(())
        }
        Err(f) => {
            let e = match &f {
                Failure::Store(e) => e.clone(),
                Failure::Rules(_) | Failure::Locked => StoreError::Aborted,
            };
            failed = Some(f);
            Err(e)
        }
    });
    match out {
        Ok(()) => Ok(done.flatten()),
        Err(StoreError::Unsupported) if failed.is_none() => {
            if aggregate_sources(env.manifest)
                .iter()
                .any(|s| s.child == entity || s.parent == entity)
            {
                return Err(Failure::Store(StoreError::Unsupported));
            }
            step(store, env, entity, op).and_then(|o| {
                audit(store, env, entity, op, &o)?;
                Ok(o.after)
            })
        }
        Err(e) => Err(failed.unwrap_or(Failure::Store(e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use ce_store::{ListRequest, MemoryStore, Page};
    use hmac::{Hmac, Mac};
    use serde_json::json;
    use sha2::Sha256;

    /// A store whose reads of child records fail like a dropped database connection.
    struct Flaky(MemoryStore, StoreError);

    impl Store for Flaky {
        fn list(&self, s: &TenantScope, e: &str, r: &ListRequest) -> Result<Page, StoreError> {
            self.0.list(s, e, r)
        }
        fn insert(
            &self,
            s: &TenantScope,
            e: &str,
            d: Value,
            k: Option<&str>,
        ) -> Result<Record, StoreError> {
            self.0.insert(s, e, d, k)
        }
        fn get(&self, s: &TenantScope, e: &str, id: &str) -> Result<Record, StoreError> {
            self.0.get(s, e, id)
        }
        fn update(
            &self,
            s: &TenantScope,
            e: &str,
            id: &str,
            v: u64,
            d: Value,
        ) -> Result<Record, StoreError> {
            self.0.update(s, e, id, v, d)
        }
        fn delete(&self, s: &TenantScope, e: &str, id: &str, v: u64) -> Result<(), StoreError> {
            self.0.delete(s, e, id, v)
        }
        fn child_values(
            &self,
            _: &TenantScope,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<Vec<Value>, StoreError> {
            Err(self.1.clone())
        }
    }

    #[test]
    fn a_failing_store_is_a_store_error_not_a_broken_rule() {
        let m = ce_manifest::validate(
            json!({
              "manifest_version": 1,
              "template": {"name": "t", "version": "0.1.0"},
              "entities": {
                "order": {"fields": {"total": {"type": "integer", "computed": "sum(children(item, order).qty)"}}},
                "item": {"fields": {"order": {"type": "ref", "ref": "order"}, "qty": {"type": "integer"}}}
              }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let key = b"0123456789abcdef0123456789abcdef";
        let (h, p) = (
            B64.encode(r#"{"alg":"HS256"}"#),
            B64.encode(r#"{"sub":"u","tenant":"11111111-1111-1111-1111-111111111111","exp":2000}"#),
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(format!("{h}.{p}").as_bytes());
        let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
        let who = ce_auth::Verifier::new(key)
            .unwrap()
            .verify_token(&token, 1000)
            .unwrap();
        let ctx = Context {
            actor_id: "u".into(),
            actor_role: None,
            now_unix: 1000,
        };
        let env = Env {
            manifest: &m,
            scope: who.scope(),
            ctx: &ctx,
            request_id: "test",
        };
        let store = Flaky(
            MemoryStore::new(),
            StoreError::Backend("connection reset".into()),
        );
        let rec = store
            .insert(who.scope(), "order", json!({"total": 0}), None)
            .unwrap();
        let op = Op::Patch {
            id: rec.id,
            expected: 1,
            patch: Map::new(),
        };
        match step(&store, &env, "order", &op) {
            Err(Failure::Store(StoreError::Backend(_))) => {}
            Err(Failure::Rules(_)) => panic!("reported as a broken rule"),
            _ => panic!("expected a store failure"),
        }
    }
}
