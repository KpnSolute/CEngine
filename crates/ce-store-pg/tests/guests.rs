//! Guest account storage on a real Postgres. Set `CE_TEST_DATABASE_URL` (and `CE_REQUIRE_DB=1` in CI).
use ce_manifest::validate;
use ce_store_pg::{GuestPgError, TypedPgStore};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";
static N: AtomicU32 = AtomicU32::new(0);

fn store() -> Option<TypedPgStore> {
    let url = match std::env::var("CE_TEST_DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            assert!(
                std::env::var("CE_REQUIRE_DB").is_err(),
                "CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not"
            );
            eprintln!("skipping: CE_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let m = validate(
        json!({"manifest_version": 1, "template": {"name": "g", "title": "G", "version": "0.1.0"},
        "entities": {"thing": {"fields": {"name": {"type": "string", "max_length": 20}}}}})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let schema = format!(
        "ce_gs_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    Some(TypedPgStore::install(&url, &m, &schema).unwrap())
}

#[test]
fn accounts_sessions_and_single_use_resets() {
    let Some(store) = store() else { return };
    let g = store.guests(T1).unwrap();
    let a = g
        .create(
            "jo@example.com",
            "Jo",
            "hash-1",
            &json!({"dept": "Admissions"}),
        )
        .unwrap();
    assert_eq!(
        g.create("jo@example.com", "Other", "hash-2", &json!({})),
        Err(GuestPgError::Exists)
    );
    assert_eq!(g.by_email("jo@example.com").unwrap().unwrap().id, a.id);
    assert_eq!(g.by_id(&a.id).unwrap().unwrap().name, "Jo");
    assert_eq!(a.profile, json!({"dept": "Admissions"}));
    g.set_profile(&a.id, "Jo Q", &json!({"dept": "Academics", "ext": "2210"}))
        .unwrap();
    let b = g.by_id(&a.id).unwrap().unwrap();
    assert_eq!(
        (b.name.as_str(), &b.profile),
        ("Jo Q", &json!({"dept": "Academics", "ext": "2210"}))
    );
    assert_eq!(
        g.set_profile("00000000-0000-4000-8000-000000000000", "x", &json!({})),
        Err(GuestPgError::Missing)
    );
    assert!(g.by_email("nobody@example.com").unwrap().is_none());
    assert!(g
        .by_id("00000000-0000-4000-8000-000000000000")
        .unwrap()
        .is_none());
    // The same email is a different account in another tenant.
    let other = store.guests(T2).unwrap();
    assert!(other.by_email("jo@example.com").unwrap().is_none());
    assert!(other
        .create("jo@example.com", "Jo", "h", &json!({}))
        .is_ok());
    assert!(other.by_id(&a.id).unwrap().is_none());

    // Sign-out and password changes leave a cutoff that a restart can read back.
    assert!(g.recently_ended(0).unwrap().is_empty());
    g.end_sessions(&a.id, 1_000).unwrap();
    assert_eq!(g.recently_ended(500).unwrap(), vec![(a.id.clone(), 1_000)]);
    assert!(g.recently_ended(1_001).unwrap().is_empty());
    g.set_password(&a.id, "hash-new", 2_000).unwrap();
    assert_eq!(g.by_id(&a.id).unwrap().unwrap().hash, "hash-new");
    assert_eq!(g.recently_ended(500).unwrap(), vec![(a.id.clone(), 2_000)]);
    assert_eq!(
        g.end_sessions("00000000-0000-4000-8000-000000000000", 1),
        Err(GuestPgError::Missing)
    );

    // A reset token works once, until it expires, and only for its own tenant.
    g.put_reset(&a.id, "tokhash", 5_000).unwrap();
    assert_eq!(other.take_reset("tokhash", 3_000).unwrap(), None);
    assert_eq!(g.take_reset("tokhash", 3_000).unwrap(), Some(a.id.clone()));
    assert_eq!(g.take_reset("tokhash", 3_000).unwrap(), None);
    g.put_reset(&a.id, "late", 5_000).unwrap();
    assert_eq!(g.take_reset("late", 5_000).unwrap(), None); // expires_at must be after now
    store.drop_schema().unwrap();
}
