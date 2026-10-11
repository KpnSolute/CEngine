//! Contract tests any `Store` backend must pass. Behind the `contract-tests` feature so the
//! signing helpers it needs stay out of normal builds. A backend calls [`run_all`] with a
//! factory that returns a fresh, empty store each time.
//!
//! The helper below builds scopes by verifying a signed token, the same way production does.

use crate::*;
use serde_json::json;

fn scope(tenant: &str) -> TenantScope {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(r#"{{"sub":"u1","tenant":"{tenant}","exp":2000}}"#));
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
    let principal = ce_auth::Verifier::new(key)
        .unwrap()
        .verify_token(&token, 1000)
        .unwrap();
    principal.scope().clone()
}

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

fn insert_then_get_round_trips<S: Store>(s: &S) {
    let r = s.insert(&scope(A), "item", json!({"n": 1}), None).unwrap();
    assert_eq!(r.version, 1);
    assert_eq!(s.get(&scope(A), "item", &r.id).unwrap(), r);
}

fn other_tenant_sees_not_found<S: Store>(s: &S) {
    let r = s.insert(&scope(A), "item", json!({"n": 1}), None).unwrap();
    assert_eq!(s.get(&scope(B), "item", &r.id), Err(StoreError::NotFound));
    assert_eq!(
        s.update(&scope(B), "item", &r.id, 1, json!({"n": 9})),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        s.delete(&scope(B), "item", &r.id, 1),
        Err(StoreError::NotFound)
    );
    // The owner's record is untouched.
    assert_eq!(
        s.get(&scope(A), "item", &r.id).unwrap().data,
        json!({"n": 1})
    );
}

fn wrong_entity_is_not_found<S: Store>(s: &S) {
    let r = s.insert(&scope(A), "item", json!({}), None).unwrap();
    assert_eq!(s.get(&scope(A), "order", &r.id), Err(StoreError::NotFound));
}

fn update_bumps_version_and_detects_conflicts<S: Store>(s: &S) {
    let r = s.insert(&scope(A), "item", json!({"n": 1}), None).unwrap();
    let r2 = s
        .update(&scope(A), "item", &r.id, 1, json!({"n": 2}))
        .unwrap();
    assert_eq!(r2.version, 2);
    assert_eq!(
        s.update(&scope(A), "item", &r.id, 1, json!({"n": 3})),
        Err(StoreError::VersionConflict { current: 2 })
    );
    assert_eq!(
        s.get(&scope(A), "item", &r.id).unwrap().data,
        json!({"n": 2})
    );
}

fn delete_is_soft_and_versioned<S: Store>(s: &S) {
    let r = s.insert(&scope(A), "item", json!({}), None).unwrap();
    assert_eq!(
        s.delete(&scope(A), "item", &r.id, 5),
        Err(StoreError::VersionConflict { current: 1 })
    );
    s.delete(&scope(A), "item", &r.id, 1).unwrap();
    assert_eq!(s.get(&scope(A), "item", &r.id), Err(StoreError::NotFound));
    assert_eq!(
        s.update(&scope(A), "item", &r.id, 2, json!({})),
        Err(StoreError::NotFound)
    );
    assert_eq!(
        s.delete(&scope(A), "item", &r.id, 2),
        Err(StoreError::NotFound)
    );
}

fn idempotency_key_replays_and_rejects_changed_payload<S: Store>(s: &S) {
    let first = s
        .insert(&scope(A), "item", json!({"n": 1}), Some("k1"))
        .unwrap();
    let again = s
        .insert(&scope(A), "item", json!({"n": 1}), Some("k1"))
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(
        s.insert(&scope(A), "item", json!({"n": 2}), Some("k1")),
        Err(StoreError::IdempotencyKeyReused)
    );
    assert_eq!(
        s.insert(&scope(A), "order", json!({"n": 1}), Some("k1")),
        Err(StoreError::IdempotencyKeyReused)
    );
}

fn idempotency_keys_are_per_tenant<S: Store>(s: &S) {
    let a = s
        .insert(&scope(A), "item", json!({"n": 1}), Some("k"))
        .unwrap();
    let b = s
        .insert(&scope(B), "item", json!({"n": 1}), Some("k"))
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(s.get(&scope(B), "item", &b.id).unwrap(), b);
}

fn non_object_data_is_refused<S: Store>(s: &S) {
    assert!(matches!(
        s.insert(&scope(A), "item", json!([1]), None),
        Err(StoreError::BadData(_))
    ));
}

fn idempotent_replay_returns_the_original_result_after_changes<S: Store>(s: &S) {
    let first = s
        .insert(&scope(A), "item", json!({"n": 1}), Some("k"))
        .unwrap();
    s.update(&scope(A), "item", &first.id, 1, json!({"n": 2}))
        .unwrap();
    assert_eq!(
        s.insert(&scope(A), "item", json!({"n": 1}), Some("k"))
            .unwrap(),
        first
    );
    s.delete(&scope(A), "item", &first.id, 2).unwrap();
    assert_eq!(
        s.insert(&scope(A), "item", json!({"n": 1}), Some("k"))
            .unwrap(),
        first
    );
}

/// Runs every contract case. `make` must return an empty store on each call.
pub fn run_all<S: Store>(make: &dyn Fn() -> S) {
    insert_then_get_round_trips(&make());
    other_tenant_sees_not_found(&make());
    wrong_entity_is_not_found(&make());
    update_bumps_version_and_detects_conflicts(&make());
    delete_is_soft_and_versioned(&make());
    idempotency_key_replays_and_rejects_changed_payload(&make());
    idempotency_keys_are_per_tenant(&make());
    non_object_data_is_refused(&make());
    idempotent_replay_returns_the_original_result_after_changes(&make());
}

// ---- Transactions and child reads. Only for backends whose `atomic` is supported. ----
//
// Entities: `parent` (field `n`, integer) and `line` (fields `parent`, a ref to a parent, and
// `n`, integer). A backend with a schema must declare both.

fn line(parent: &str, n: i64) -> Value {
    json!({"parent": parent, "n": n})
}

fn sorted_ints(mut v: Vec<Value>) -> Vec<i64> {
    let mut out: Vec<i64> = v.drain(..).filter_map(|x| x.as_i64()).collect();
    out.sort();
    out
}

fn atomic_commits_everything_and_sees_its_own_writes<S: Store>(s: &S) {
    let (mut pid, mut lid) = (String::new(), String::new());
    s.atomic(&mut |tx| {
        let p = tx.insert(&scope(A), "parent", json!({"n": 0}), None)?;
        let l = tx.insert(&scope(A), "line", line(&p.id, 5), None)?;
        // Reads inside see the writes made so far.
        assert_eq!(tx.get(&scope(A), "line", &l.id)?, l);
        assert_eq!(
            sorted_ints(tx.child_values(&scope(A), "line", "parent", &p.id, Some("n"))?),
            vec![5]
        );
        tx.update(&scope(A), "parent", &p.id, 1, json!({"n": 5}))?;
        assert_eq!(tx.get_locked(&scope(A), "parent", &p.id)?.version, 2);
        pid = p.id;
        lid = l.id;
        Ok(())
    })
    .unwrap();
    assert_eq!(s.get(&scope(A), "parent", &pid).unwrap().data["n"], 5);
    assert_eq!(s.get(&scope(A), "line", &lid).unwrap().version, 1);
}

fn atomic_rolls_back_everything_on_error<S: Store>(s: &S) {
    let keep = s
        .insert(&scope(A), "parent", json!({"n": 1}), None)
        .unwrap();
    let mut made = String::new();
    let out = s.atomic(&mut |tx| {
        let l = tx.insert(&scope(A), "line", line(&keep.id, 9), None)?;
        made = l.id;
        tx.update(&scope(A), "parent", &keep.id, 1, json!({"n": 99}))?;
        tx.delete(&scope(A), "parent", &keep.id, 2)?;
        Err(StoreError::Aborted)
    });
    assert_eq!(out, Err(StoreError::Aborted));
    // Nothing the closure did is visible: not the insert, the update or the delete.
    assert_eq!(s.get(&scope(A), "line", &made), Err(StoreError::NotFound));
    assert_eq!(s.get(&scope(A), "parent", &keep.id).unwrap(), keep);
    assert!(s
        .child_values(&scope(A), "line", "parent", &keep.id, None)
        .unwrap()
        .is_empty());
    // A store error from a statement inside also rolls everything back.
    let out = s.atomic(&mut |tx| {
        tx.insert(&scope(A), "line", line(&keep.id, 1), None)?;
        tx.update(&scope(A), "parent", &keep.id, 7, json!({"n": 2}))?;
        Ok(())
    });
    assert_eq!(out, Err(StoreError::VersionConflict { current: 1 }));
    assert!(s
        .child_values(&scope(A), "line", "parent", &keep.id, None)
        .unwrap()
        .is_empty());
}

fn nested_atomic_joins_the_outer_transaction<S: Store>(s: &S) {
    let p = s
        .insert(&scope(A), "parent", json!({"n": 0}), None)
        .unwrap();
    let out = s.atomic(&mut |tx| {
        tx.atomic(&mut |inner| {
            inner.insert(&scope(A), "line", line(&p.id, 1), None)?;
            Ok(())
        })?;
        // The inner closure succeeded, but the outer one still decides.
        Err(StoreError::Aborted)
    });
    assert_eq!(out, Err(StoreError::Aborted));
    assert!(s
        .child_values(&scope(A), "line", "parent", &p.id, None)
        .unwrap()
        .is_empty());
}

fn child_values_read_live_children_of_one_parent_and_tenant<S: Store>(s: &S) {
    let p1 = s.insert(&scope(A), "parent", json!({}), None).unwrap();
    let p2 = s.insert(&scope(A), "parent", json!({}), None).unwrap();
    let q = s.insert(&scope(B), "parent", json!({}), None).unwrap();
    s.insert(&scope(A), "line", line(&p1.id, 1), None).unwrap();
    s.insert(&scope(A), "line", line(&p1.id, 2), None).unwrap();
    let gone = s.insert(&scope(A), "line", line(&p1.id, 4), None).unwrap();
    s.insert(&scope(A), "line", line(&p2.id, 8), None).unwrap();
    s.insert(&scope(B), "line", line(&q.id, 16), None).unwrap();
    s.delete(&scope(A), "line", &gone.id, 1).unwrap();
    let vals = |scope: &TenantScope, p: &str, attr| {
        s.child_values(scope, "line", "parent", p, attr).unwrap()
    };
    assert_eq!(sorted_ints(vals(&scope(A), &p1.id, Some("n"))), vec![1, 2]);
    assert_eq!(
        vals(&scope(A), &p1.id, None),
        vec![json!(true), json!(true)]
    );
    assert_eq!(sorted_ints(vals(&scope(A), &p2.id, Some("n"))), vec![8]);
    // Another tenant's parent id finds nothing, and its own children stay its own.
    assert!(vals(&scope(B), &p1.id, Some("n")).is_empty());
    assert_eq!(sorted_ints(vals(&scope(B), &q.id, Some("n"))), vec![16]);
}

/// Runs the transaction and child-read cases. `make` must return an empty store on each call.
pub fn run_atomic<S: Store>(make: &dyn Fn() -> S) {
    atomic_commits_everything_and_sees_its_own_writes(&make());
    atomic_rolls_back_everything_on_error(&make());
    nested_atomic_joins_the_outer_transaction(&make());
    child_values_read_live_children_of_one_parent_and_tenant(&make());
}
