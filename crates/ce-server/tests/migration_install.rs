//! Fresh install, then migrate: the history is complete and `migrate` has nothing to do.
use ce_manifest::validate;
use ce_store_pg::migrations::{embedded, MigrateError};
use ce_store_pg::TypedPgStore;
use serde_json::json;
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

fn manifest() -> ce_manifest::Manifest {
    validate(
        json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"doc": {"fields": {"title": {"type": "string", "required": true}}}},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

fn schema() -> String {
    format!(
        "ce_mi_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

#[test]
fn a_fresh_install_records_every_migration_and_migrate_is_a_no_op() {
    let Some(url) = url() else { return };
    let m = manifest();
    let sch = schema();
    let store = TypedPgStore::install(&url, &m, &sch).unwrap();
    let list = embedded();

    let hist = store.migration_history().unwrap();
    assert_eq!(
        hist.len(),
        list.len(),
        "one history row per embedded migration"
    );
    for (row, mig) in hist.iter().zip(&list) {
        assert_eq!(row.version, mig.version);
        assert_eq!(row.name, mig.name);
        assert_eq!(row.sha256, mig.checksum());
        assert_eq!(row.status, "applied");
        assert_eq!(row.applied_by, "install");
        assert_eq!(row.manifest_hash, m.hash);
    }

    let plan = store.migration_plan(&list).unwrap();
    assert_eq!(plan.current, list.len() as u32);
    assert!(plan.pending.is_empty());

    let done = store.migrate(&list, "tester").unwrap();
    assert!(
        done.pending.is_empty(),
        "nothing applied on a current schema"
    );
    assert_eq!(store.migration_history().unwrap().len(), list.len());
}

#[test]
fn a_fresh_install_has_the_provenance_tables() {
    let Some(url) = url() else { return };
    let sch = schema();
    let store = TypedPgStore::install(&url, &manifest(), &sch).unwrap();
    let mut c = ce_store_pg::connect(&url, &ce_store_pg::Tls::Off).unwrap();
    for t in [
        "migration_runs",
        "migration_id_map",
        "migration_reconciliation_results",
        "migration_errors",
        "migration_artifacts",
        "_ce_migration",
    ] {
        let found: bool = c
            .query_one(
                "SELECT to_regclass($1) IS NOT NULL",
                &[&format!(r#""{sch}"."{t}""#)],
            )
            .unwrap()
            .get(0);
        assert!(found, "{t} should exist after install");
    }
    drop(store);
}

#[test]
fn a_schema_without_history_is_refused_until_adopted() {
    let Some(url) = url() else { return };
    let sch = schema();
    let store = TypedPgStore::install(&url, &manifest(), &sch).unwrap();
    let mut c = ce_store_pg::connect(&url, &ce_store_pg::Tls::Off).unwrap();
    c.batch_execute(&format!(r#"DROP TABLE "{sch}"."_ce_migration""#))
        .unwrap();
    let list = embedded();
    assert_eq!(store.migration_plan(&list), Err(MigrateError::NoHistory));
    assert_eq!(store.migrate(&list, "tester"), Err(MigrateError::NoHistory));
    store.adopt_migration_baseline(&list, "tester").unwrap();
    let plan = store.migration_plan(&list).unwrap();
    assert_eq!(plan.current, 1);
    assert_eq!(plan.pending.len(), list.len() - 1);
    assert!(
        store.adopt_migration_baseline(&list, "tester").is_err(),
        "adopting twice is refused"
    );
}

#[test]
fn a_schema_installed_at_an_older_version_migrates_forward_to_current() {
    let Some(url) = url() else { return };
    let (m, sch, list) = (manifest(), schema(), embedded());
    let store = TypedPgStore::install_with_migrations(
        &url,
        &m,
        &sch,
        ce_store_pg::PoolConfig::default(),
        &list[..1],
    )
    .unwrap();
    assert_eq!(store.migration_plan(&list).unwrap().current, 1);

    let done = store.migrate(&list, "tester").unwrap();
    assert_eq!(done.pending, vec![(2, "migration_provenance".to_string())]);

    let hist = store.migration_history().unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].applied_by, "install");
    let step = &hist[1];
    assert_eq!(
        (step.version, step.status.as_str(), step.applied_by.as_str()),
        (2, "applied", "tester")
    );
    assert_eq!(
        step.sha256,
        list[1].checksum(),
        "the recorded checksum is the SQL's"
    );
    assert_eq!(step.manifest_hash, m.hash);
    assert_eq!(store.migration_plan(&list).unwrap().current, 2);
}
