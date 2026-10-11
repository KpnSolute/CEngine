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
use std::cell::RefCell;
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
    /// The first store failure met while answering. The evaluator only sees a message, so this
    /// keeps the real error: a backend or unsupported-store failure is not a broken rule.
    failed: RefCell<Option<StoreError>>,
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
        match self.store.get(self.env.scope, target, id) {
            Ok(rec) => Ok(rec
                .data
                .get(attr)
                .map_or(Ev::Null, |v| json_to_expr(field_type(m, target, attr), v))),
            Err(StoreError::NotFound) => Ok(Ev::Null),
            Err(e) => Err(self.fail(e)),
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
            .map_err(|e| self.fail(e))?;
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
    };
    let out = evaluate_computed_with(env.manifest, entity, record, old, env.ctx, Some(&r))
        .and_then(|()| check_invariants_with(env.manifest, entity, record, old, env.ctx, Some(&r)));
    match (out, r.failed.into_inner()) {
        (_, Some(e)) => Err(Failure::Store(e)),
        (Err(errors), None) => Err(Failure::Rules(errors)),
        (Ok(()), None) => Ok(()),
    }
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
            // Retries are matched on the body as sent, not on values derived from other records.
            obj.retain(|k, v| data.get(k) != Some(&*v));
            let rec = tx.insert_derived(
                env.scope,
                entity,
                Value::Object(data.clone()),
                obj,
                Some(key),
            )?;
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
