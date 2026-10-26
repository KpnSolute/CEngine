//! Migration roles (SOP section 4): a SELECT-only legacy reader, a writer limited to its own
//! schema, and a runtime role that cannot change the schema or the migration history.
use ce_manifest::validate;
use ce_store_pg::privileges::{reader_sql, target_sql, verify_reader_read_only};
use ce_store_pg::{connect, Client, Tls, TypedPgStore};

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

/// Schemas and roles for one test, removed again when it ends (also when it panics).
struct Rig {
    url: String,
    legacy: String,
    target: String,
    other: String,
    reader: String,
    writer: String,
    runtime: String,
    _stores: Vec<TypedPgStore>,
}

impl Rig {
    fn new(url: &str) -> Rig {
        let id = format!(
            "{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let r = Rig {
            url: url.to_string(),
            legacy: format!("ce_pl_{id}"),
            target: format!("ce_pt_{id}"),
            other: format!("ce_po_{id}"),
            reader: format!("ce_rd_{id}"),
            writer: format!("ce_wr_{id}"),
            runtime: format!("ce_rt_{id}"),
            _stores: Vec::new(),
        };
        let mut c = connect(url, &Tls::Off).unwrap();
        for role in [&r.reader, &r.writer, &r.runtime] {
            c.batch_execute(&format!(r#"CREATE ROLE "{role}" NOLOGIN"#))
                .unwrap();
        }
        c.batch_execute(&format!(
            r#"CREATE SCHEMA "{l}";
               CREATE TABLE "{l}".inventory_items (id int PRIMARY KEY, name text);
               INSERT INTO "{l}".inventory_items VALUES (1, 'rice');"#,
            l = r.legacy
        ))
        .unwrap();
        r
    }

    fn su(&self) -> Client {
        connect(&self.url, &Tls::Off).unwrap()
    }

    /// Runs `sql` as `role`; Ok(()) or the error text.
    fn as_role(&self, role: &str, sql: &str) -> Result<(), String> {
        let mut c = self.su();
        c.batch_execute(&format!(r#"SET ROLE "{role}""#)).unwrap();
        c.batch_execute(sql).map_err(|e| {
            e.as_db_error()
                .map_or_else(|| e.to_string(), |d| d.message().to_string())
        })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let mut c = connect(&self.url, &Tls::Off).unwrap();
        for s in [&self.legacy, &self.target, &self.other] {
            let _ = c.batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{s}" CASCADE"#));
        }
        for role in [&self.reader, &self.writer, &self.runtime] {
            let _ = c.batch_execute(&format!(r#"DROP OWNED BY "{role}""#));
            let _ = c.batch_execute(&format!(r#"DROP ROLE IF EXISTS "{role}""#));
        }
    }
}

fn denied(r: Result<(), String>) -> bool {
    matches!(r, Err(e) if e.contains("permission denied") || e.contains("must be owner"))
}

fn setup(url: &str) -> Rig {
    let mut r = Rig::new(url);
    r._stores
        .push(TypedPgStore::install(url, &manifest(), &r.target).unwrap());
    r._stores
        .push(TypedPgStore::install(url, &manifest(), &r.other).unwrap());
    let mut c = r.su();
    c.batch_execute(&reader_sql(&r.legacy, &r.reader).unwrap())
        .unwrap();
    c.batch_execute(&target_sql(&r.target, &r.writer, &r.runtime).unwrap())
        .unwrap();
    r
}

#[test]
fn the_legacy_reader_can_select_and_nothing_else() {
    let Some(url) = url() else { return };
    let r = setup(&url);
    let l = &r.legacy;
    assert_eq!(
        r.as_role(
            &r.reader,
            &format!(r#"SELECT * FROM "{l}".inventory_items"#)
        ),
        Ok(())
    );
    for sql in [
        format!(r#"INSERT INTO "{l}".inventory_items VALUES (2, 'x')"#),
        format!(r#"UPDATE "{l}".inventory_items SET name = 'x'"#),
        format!(r#"DELETE FROM "{l}".inventory_items"#),
        format!(r#"TRUNCATE "{l}".inventory_items"#),
        format!(r#"CREATE TABLE "{l}".sneaky (a int)"#),
        format!(r#"DROP TABLE "{l}".inventory_items"#),
    ] {
        assert!(denied(r.as_role(&r.reader, &sql)), "reader ran: {sql}");
    }
    // It cannot see the target at all.
    assert!(denied(r.as_role(
        &r.reader,
        &format!(r#"SELECT * FROM "{}".migration_runs"#, r.target)
    )));
}

#[test]
fn a_broader_reader_is_detected_so_the_migration_can_fail_closed() {
    let Some(url) = url() else { return };
    let r = setup(&url);
    let mut c = r.su();
    assert_eq!(
        verify_reader_read_only(&mut c, &r.reader, &[&r.legacy]),
        Ok(())
    );
    c.batch_execute(&format!(
        r#"GRANT UPDATE ON "{}".inventory_items TO "{}""#,
        r.legacy, r.reader
    ))
    .unwrap();
    c.batch_execute(&format!(
        r#"GRANT CREATE ON SCHEMA "{}" TO "{}""#,
        r.legacy, r.reader
    ))
    .unwrap();
    let bad = verify_reader_read_only(&mut c, &r.reader, &[&r.legacy]).unwrap_err();
    assert!(bad.iter().any(|m| m.contains("UPDATE")), "{bad:?}");
    assert!(
        bad.iter().any(|m| m.contains("CREATE in schema")),
        "{bad:?}"
    );
}

#[test]
fn the_target_writer_is_limited_to_its_own_schema() {
    let Some(url) = url() else { return };
    let r = setup(&url);
    let (t, o, l) = (&r.target, &r.other, &r.legacy);
    let run = "11111111-1111-1111-1111-111111111111";
    let ins = format!(
        r#"INSERT INTO "{t}".migration_runs (id, tenant_id, installation_id, source_system, source_project_ref, migration_version, initiated_by)
           VALUES ('{run}', '{run}', 'i', 'mjcc', 'ref', '1', 'tester')"#
    );
    assert_eq!(r.as_role(&r.writer, &ins), Ok(()));
    assert_eq!(
        r.as_role(&r.writer, &format!(r#"CREATE TABLE "{t}".extra (a int)"#)),
        Ok(())
    );
    assert!(
        denied(r.as_role(&r.writer, &format!(r#"SELECT * FROM "{o}".migration_runs"#))),
        "other tenant schema"
    );
    assert!(denied(r.as_role(
        &r.writer,
        &format!(r#"CREATE TABLE "{o}".extra (a int)"#)
    )));
    assert!(
        denied(r.as_role(
            &r.writer,
            &format!(r#"SELECT * FROM "{l}".inventory_items"#)
        )),
        "legacy"
    );
    assert!(denied(r.as_role(
        &r.writer,
        &format!(r#"DELETE FROM "{l}".inventory_items"#)
    )));
}

#[test]
fn the_runtime_role_cannot_change_the_schema_or_the_migration_history() {
    let Some(url) = url() else { return };
    let r = setup(&url);
    let (t, o) = (&r.target, &r.other);
    let rt = &r.runtime;
    assert_eq!(
        r.as_role(rt, &format!(r#"SELECT count(*) FROM "{t}".migration_runs"#)),
        Ok(())
    );
    assert_eq!(
        r.as_role(rt, &format!(r#"SELECT count(*) FROM "{t}"._ce_migration"#)),
        Ok(())
    );
    for sql in [
        format!(r#"CREATE TABLE "{t}".extra (a int)"#),
        format!(r#"DROP TABLE "{t}".migration_runs"#),
        format!(r#"DELETE FROM "{t}"._ce_migration"#),
        format!(r#"UPDATE "{t}"._ce_migration SET sha256 = 'x'"#),
        format!(r#"DELETE FROM "{t}".migration_runs"#),
        format!(
            r#"INSERT INTO "{t}".migration_errors (tenant_id, migration_run_id, source_table, source_id, error_code, safe_error_message, severity) VALUES (gen_random_uuid(), gen_random_uuid(), 't', '1', 'c', 'm', 'error')"#
        ),
        format!(r#"SELECT * FROM "{o}".migration_runs"#),
    ] {
        assert!(denied(r.as_role(rt, &sql)), "runtime ran: {sql}");
    }
}

#[test]
fn the_writer_and_the_runtime_role_must_differ() {
    assert!(target_sql("ce_x", "same", "same").is_err());
    assert!(target_sql("ce_x; DROP", "w", "r").is_err());
    assert!(reader_sql("bad schema", "r").is_err());
}
