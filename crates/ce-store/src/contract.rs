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
