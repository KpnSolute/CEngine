//! Contract tests. A future Postgres backend must pass the same cases.
use ce_store::*;
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

#[test]
fn insert_then_get_round_trips() {
    let s = MemoryStore::new();
    let r = s.insert(&scope(A), "item", json!({"n": 1}), None).unwrap();
    assert_eq!(r.version, 1);
    assert_eq!(s.get(&scope(A), "item", &r.id).unwrap(), r);
}

#[test]
fn other_tenant_sees_not_found() {
    let s = MemoryStore::new();
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

#[test]
fn wrong_entity_is_not_found() {
    let s = MemoryStore::new();
    let r = s.insert(&scope(A), "item", json!({}), None).unwrap();
    assert_eq!(s.get(&scope(A), "order", &r.id), Err(StoreError::NotFound));
}

#[test]
fn update_bumps_version_and_detects_conflicts() {
    let s = MemoryStore::new();
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

#[test]
fn delete_is_soft_and_versioned() {
    let s = MemoryStore::new();
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

#[test]
fn idempotency_key_replays_and_rejects_changed_payload() {
    let s = MemoryStore::new();
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

#[test]
fn idempotency_keys_are_per_tenant() {
    let s = MemoryStore::new();
    let a = s
        .insert(&scope(A), "item", json!({"n": 1}), Some("k"))
        .unwrap();
    let b = s
        .insert(&scope(B), "item", json!({"n": 1}), Some("k"))
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(s.get(&scope(B), "item", &b.id).unwrap(), b);
}

#[test]
fn non_object_data_is_refused() {
    let s = MemoryStore::new();
    assert!(matches!(
        s.insert(&scope(A), "item", json!([1]), None),
        Err(StoreError::BadData(_))
    ));
}
