//! Runs the shared contract on a real Postgres. Set `CE_TEST_DATABASE_URL`. In CI also set
//! `CE_REQUIRE_DB=1` so a missing database fails the test instead of silently skipping it.
use ce_store::{contract, StoreError};
use ce_store_pg::PgStore;
use std::sync::atomic::{AtomicU32, Ordering};

fn url() -> Option<String> {
    match std::env::var("CE_TEST_DATABASE_URL") {
        Ok(u) => Some(u),
        Err(_) => {
            assert!(
                std::env::var("CE_REQUIRE_DB").is_err(),
                "CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not"
            );
            eprintln!("skipping: CE_TEST_DATABASE_URL not set");
            None
        }
    }
}

static N: AtomicU32 = AtomicU32::new(0);

fn fresh(url: &str) -> PgStore {
    let name = format!(
        "ce_test_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    PgStore::connect(url, &name).expect("connect")
}

#[test]
fn postgres_store_passes_the_contract() {
    let Some(url) = url() else { return };
    let schemas: std::cell::RefCell<Vec<PgStore>> = Default::default();
    // The factory hands out a fresh schema per case; we keep one extra handle to drop it after.
    contract::run_all(&|| {
        let s = fresh(&url);
        schemas.borrow_mut().push(fresh_handle(&url, &s));
        s
    });
    for s in schemas.borrow().iter() {
        s.drop_schema().unwrap();
    }
}

/// A second connection to the same schema, used only to clean up.
fn fresh_handle(url: &str, like: &PgStore) -> PgStore {
    PgStore::connect(url, like.schema_name()).unwrap()
}

#[test]
fn bad_schema_names_are_refused() {
    let Some(url) = url() else { return };
    for bad in ["", "Bad", "a-b", "1abc", "a\"; drop schema public; --"] {
        assert!(
            matches!(PgStore::connect(&url, bad), Err(StoreError::BadData(_))),
            "{bad}"
        );
    }
}

fn scope() -> ce_store::TenantScope {
    // Built by verifying a signed token, like production.
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p =
        B64.encode(r#"{"sub":"u1","tenant":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","exp":2000}"#);
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
    ce_auth::Verifier::new(key)
        .unwrap()
        .verify_token(&token, 1000)
        .unwrap()
        .scope()
        .clone()
}

#[test]
fn concurrent_updates_at_one_version_have_exactly_one_winner() {
    use ce_store::Store;
    let Some(url) = url() else { return };
    let a = fresh(&url);
    let b = PgStore::connect(&url, a.schema_name()).unwrap();
    let sc = scope();
    let rec = a
        .insert(&sc, "item", serde_json::json!({"n": 0}), None)
        .unwrap();
    for round in 0..20 {
        let cur = a.get(&sc, "item", &rec.id).unwrap().version;
        let (ra, rb) = std::thread::scope(|t| {
            let x = t.spawn(|| a.update(&sc, "item", &rec.id, cur, serde_json::json!({"w": "a"})));
            let y = t.spawn(|| b.update(&sc, "item", &rec.id, cur, serde_json::json!({"w": "b"})));
            (x.join().unwrap(), y.join().unwrap())
        });
        let wins = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
        assert_eq!(wins, 1, "round {round}: {ra:?} {rb:?}");
        let loser = if ra.is_err() { ra } else { rb };
        assert!(matches!(loser, Err(StoreError::VersionConflict { .. })));
    }
    a.drop_schema().unwrap();
}

#[test]
fn concurrent_inserts_with_one_key_create_one_record() {
    use ce_store::Store;
    let Some(url) = url() else { return };
    let a = fresh(&url);
    let b = PgStore::connect(&url, a.schema_name()).unwrap();
    let sc = scope();
    for i in 0..20 {
        let key = format!("k{i}");
        let (ra, rb) = std::thread::scope(|t| {
            let x = t.spawn(|| a.insert(&sc, "item", serde_json::json!({"n": i}), Some(&key)));
            let y = t.spawn(|| b.insert(&sc, "item", serde_json::json!({"n": i}), Some(&key)));
            (x.join().unwrap().unwrap(), y.join().unwrap().unwrap())
        });
        assert_eq!(ra, rb, "key {key} gave two different records");
    }
    a.drop_schema().unwrap();
}
