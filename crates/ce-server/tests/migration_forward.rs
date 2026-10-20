//! Forward migration contract: order, checksum pinning, gaps, a database ahead of the binary,
//! destructive SQL, `--plan` writing nothing, and a failing migration leaving a diagnosis.
use ce_manifest::validate;
use ce_store_pg::migrations::{embedded, is_destructive, MigrateError, Migration};
use ce_store_pg::{PoolConfig, Tls, TypedPgStore};
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
        "ce_mf_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

/// A schema installed at the previous supported version (only the baseline recorded).
fn previous(url: &str) -> (TypedPgStore, String) {
    let sch = schema();
    let store = TypedPgStore::install_with_migrations(
        url,
        &manifest(),
        &sch,
        PoolConfig::default(),
        &embedded()[..1],
    )
    .unwrap();
    (store, sch)
}

fn has_table(url: &str, sch: &str, t: &str) -> bool {
    let mut c = ce_store_pg::connect(url, &Tls::Off).unwrap();
    c.query_one(
        "SELECT to_regclass($1) IS NOT NULL",
        &[&format!(r#""{sch}"."{t}""#)],
    )
    .unwrap()
    .get(0)
}

#[test]
fn migrate_applies_pending_versions_in_order_and_records_them() {
    let Some(url) = url() else { return };
    let (store, sch) = previous(&url);
    assert!(!has_table(&url, &sch, "migration_runs"));
    let list = embedded();
    let done = store.migrate(&list, "tester").unwrap();
    assert_eq!(done.current, 1);
    assert_eq!(done.pending, vec![(2, "migration_provenance".to_string())]);
    assert!(has_table(&url, &sch, "migration_runs"));

    let hist = store.migration_history().unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[1].version, 2);
    assert_eq!(hist[1].applied_by, "tester");
    assert_eq!(hist[1].sha256, list[1].checksum());
    assert_eq!(hist[1].manifest_hash, manifest().hash);
}

#[test]
fn a_current_schema_is_a_no_op() {
    let Some(url) = url() else { return };
    let (store, _) = previous(&url);
    let list = embedded();
    store.migrate(&list, "tester").unwrap();
    let before = store.migration_history().unwrap();
    let again = store.migrate(&list, "tester").unwrap();
    assert!(again.pending.is_empty());
    assert_eq!(store.migration_history().unwrap(), before);
}

#[test]
fn plan_reports_pending_and_writes_nothing() {
    let Some(url) = url() else { return };
    let (store, sch) = previous(&url);
    let before = store.migration_history().unwrap();
    let plan = store.migration_plan(&embedded()).unwrap();
    assert_eq!(plan.current, 1);
    assert_eq!(plan.pending.len(), 1);
    assert_eq!(store.migration_history().unwrap(), before);
    assert!(
        !has_table(&url, &sch, "migration_runs"),
        "plan must not create tables"
    );
}

#[test]
fn a_changed_checksum_stops_everything() {
    let Some(url) = url() else { return };
    let (store, _) = previous(&url);
    store.migrate(&embedded(), "tester").unwrap();
    let mut edited = embedded();
    edited[1].sql.push_str("\n-- edited after it was applied\n");
    edited.push(Migration::new(3, "next", "SELECT 1;"));
    let err = store.migration_plan(&edited).unwrap_err();
    assert_eq!(err, MigrateError::ChecksumChanged { version: 2 });
    assert_eq!(store.migrate(&edited, "tester"), Err(err));
    assert_eq!(
        store.migration_history().unwrap().len(),
        2,
        "version 3 was not applied"
    );
}

#[test]
fn a_gap_in_the_history_fails_closed() {
    let Some(url) = url() else { return };
    let (store, sch) = previous(&url);
    store.migrate(&embedded(), "tester").unwrap();
    let mut c = ce_store_pg::connect(&url, &Tls::Off).unwrap();
    c.batch_execute(&format!(
        r#"DELETE FROM "{sch}"."_ce_migration" WHERE version = 1"#
    ))
    .unwrap();
    assert_eq!(
        store.migration_plan(&embedded()),
        Err(MigrateError::Gap { missing: 1 })
    );
}

#[test]
fn a_database_ahead_of_the_binary_fails_closed() {
    let Some(url) = url() else { return };
    let (store, _) = previous(&url);
    store.migrate(&embedded(), "tester").unwrap();
    // An older binary only knows version 1.
    let old_binary = embedded()[..1].to_vec();
    assert_eq!(
        store.migration_plan(&old_binary),
        Err(MigrateError::AheadOfBinary {
            database: 2,
            binary: 1
        })
    );
    assert!(store.migrate(&old_binary, "tester").is_err());
}

#[test]
fn destructive_sql_is_refused_before_anything_is_written() {
    let Some(url) = url() else { return };
    let (store, sch) = previous(&url);
    let mut list = embedded();
    list.push(Migration::new(
        3,
        "tidy",
        r#"ALTER TABLE "__SCHEMA__"."migration_runs" DROP COLUMN notes_safe;"#,
    ));
    let err = store.migrate(&list, "tester").unwrap_err();
    assert_eq!(err, MigrateError::Destructive { version: 3 });
    // Neither the safe version 2 nor the refused version 3 was applied.
    assert_eq!(store.migration_history().unwrap().len(), 1);
    assert!(!has_table(&url, &sch, "migration_runs"));
    list[2].sql = "TRUNCATE \"__SCHEMA__\".\"doc\";".into();
    assert!(matches!(
        store.migration_plan(&list),
        Err(MigrateError::Destructive { version: 3 })
    ));
}

#[test]
fn only_real_drop_and_truncate_statements_count_as_destructive() {
    assert!(is_destructive("DROP TABLE x;"));
    assert!(is_destructive("alter table t\n  drop column c;"));
    assert!(is_destructive("TRUNCATE t"));
    assert!(!is_destructive("CREATE TABLE t (dropped_at timestamptz);"));
    assert!(!is_destructive(
        "-- drop table mentioned in a comment\nSELECT 1;"
    ));
}

#[test]
fn a_failing_migration_rolls_back_and_leaves_a_diagnosis() {
    let Some(url) = url() else { return };
    let (store, sch) = previous(&url);
    store.migrate(&embedded(), "tester").unwrap();
    let mut list = embedded();
    list.push(Migration::new(
        3,
        "half_done",
        r#"CREATE TABLE "__SCHEMA__"."scratch" (id int);
           INSERT INTO "__SCHEMA__"."no_such_table" VALUES (1);"#,
    ));
    let err = store.migrate(&list, "tester").unwrap_err();
    let MigrateError::Failed { version, message } = err else {
        panic!("expected Failed, got {err:?}")
    };
    assert_eq!(version, 3);
    assert!(message.contains("no_such_table"), "{message}");
    assert!(
        !has_table(&url, &sch, "scratch"),
        "the half-done table was rolled back"
    );

    let hist = store.migration_history().unwrap();
    let failed = hist.last().unwrap();
    assert_eq!((failed.version, failed.status.as_str()), (3, "failed"));
    // A failed row does not count as applied: the schema is still at version 2 ...
    assert_eq!(store.migration_plan(&embedded()).unwrap().current, 2);
    // ... and a corrected version 3 applies.
    list[2] = Migration::new(
        3,
        "fixed",
        r#"CREATE TABLE "__SCHEMA__"."scratch" (id int);"#,
    );
    store.migrate(&list, "tester").unwrap();
    assert!(has_table(&url, &sch, "scratch"));
}

#[test]
fn a_binary_list_that_is_not_in_order_is_rejected() {
    let Some(url) = url() else { return };
    let (store, _) = previous(&url);
    let mut list = embedded();
    list.swap(0, 1);
    assert!(matches!(
        store.migration_plan(&list),
        Err(MigrateError::BadList(_))
    ));
}
