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
use serde_json::{Map, Value};
use std::collections::BTreeSet;

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
}

/// Answers `ref()` and `children()` from the store, for one record of one tenant.
struct StoreResolver<'a> {
    store: &'a dyn Store,
    env: &'a Env<'a>,
    entity: &'a str,
    /// None while the record is being created: it has no children yet.
    id: Option<&'a str>,
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
        match self.store.get(self.env.scope, target, id) {
            Ok(rec) => Ok(rec
                .data
                .get(attr)
                .map_or(Ev::Null, |v| json_to_expr(field_type(m, target, attr), v))),
            Err(StoreError::NotFound) => Ok(Ev::Null),
            Err(e) => Err(format!("{e:?}")),
        }
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
        let values = self
            .store
            .child_values(self.env.scope, entity, ref_field, id, attr)
            .map_err(|e| format!("{e:?}"))?;
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
    };
    evaluate_computed_with(env.manifest, entity, record, old, env.ctx, Some(&r))
        .and_then(|()| check_invariants_with(env.manifest, entity, record, old, env.ctx, Some(&r)))
        .map_err(Failure::Rules)
}

/// Brings every parent of a changed child up to date. `old` and `new` are the child's data
/// before and after the write (`None` when it did not exist).
fn refresh_parents(
    tx: &dyn Store,
    env: &Env,
    child: &str,
    old: Option<&Map<String, Value>>,
    new: Option<&Map<String, Value>>,
    depth: usize,
) -> Result<(), Failure> {
    for src in aggregate_sources(env.manifest)
        .into_iter()
        .filter(|s| s.child == child)
    {
        let ids: BTreeSet<&str> = [old, new]
            .into_iter()
            .flatten()
            .filter_map(|d| d.get(&src.ref_field).and_then(Value::as_str))
            .collect();
        if ids.is_empty() {
            continue;
        }
        if depth >= MAX_DEPTH {
            return Err(Failure::Store(StoreError::Constraint("aggregate depth")));
        }
        for id in ids {
            let current = match tx.get_locked(env.scope, &src.parent, id) {
                Ok(r) => r,
                // A dangling reference has no total to keep.
                Err(StoreError::NotFound) => continue,
                Err(e) => return Err(e.into()),
            };
            let Value::Object(before) = &current.data else {
                continue;
            };
            let mut after = before.clone();
            settle(tx, env, &src.parent, Some(id), &mut after, Some(before))?;
            if &after != before {
                tx.update(
                    env.scope,
                    &src.parent,
                    id,
                    current.version,
                    Value::Object(after.clone()),
                )?;
                refresh_parents(tx, env, &src.parent, Some(before), Some(&after), depth + 1)?;
            }
        }
    }
    Ok(())
}

fn step(tx: &dyn Store, env: &Env, entity: &str, op: &Op) -> Result<Option<Record>, Failure> {
    match op {
        Op::Create { data, key } => {
            let mut obj = data.clone();
            settle(tx, env, entity, None, &mut obj, None)?;
            let rec = tx.insert(env.scope, entity, Value::Object(obj), Some(key))?;
            refresh_parents(tx, env, entity, None, rec.data.as_object(), 0)?;
            Ok(Some(rec))
        }
        Op::Patch {
            id,
            expected,
            patch,
        } => {
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
            settle(tx, env, entity, Some(id), &mut obj, old)?;
            let rec = tx.update(env.scope, entity, id, *expected, Value::Object(obj))?;
            refresh_parents(tx, env, entity, old, rec.data.as_object(), 0)?;
            Ok(Some(rec))
        }
        Op::Delete { id, expected } => {
            let current = tx.get_locked(env.scope, entity, id)?;
            tx.delete(env.scope, entity, id, *expected)?;
            refresh_parents(tx, env, entity, current.data.as_object(), None, 0)?;
            Ok(None)
        }
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
    let out = store.atomic(&mut |tx| match step(tx, env, entity, op) {
        Ok(rec) => {
            done = Some(rec);
            Ok(())
        }
        Err(f) => {
            let e = match &f {
                Failure::Store(e) => e.clone(),
                Failure::Rules(_) => StoreError::Aborted,
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
            step(store, env, entity, op)
        }
        Err(e) => Err(failed.unwrap_or(Failure::Store(e))),
    }
}
