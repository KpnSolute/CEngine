//! Roles for a historical migration (SOP section 4): a legacy reader that can only SELECT, a
//! target writer limited to one installation schema, and a runtime role that cannot change the
//! schema or the migration history. The SQL is generated, not run: an operator applies it with
//! a privileged login, and [`verify_reader_read_only`] lets a migration fail closed when the
//! reader it was given is broader than that.

use postgres::Client;

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

fn check(names: &[&str]) -> Result<(), String> {
    match names.iter().find(|n| !ident_ok(n)) {
        Some(n) => Err(format!("invalid identifier {n:?}")),
        None => Ok(()),
    }
}

/// SELECT on every table of `legacy_schema` and nothing else.
pub fn reader_sql(legacy_schema: &str, reader: &str) -> Result<String, String> {
    check(&[legacy_schema, reader])?;
    Ok(format!(
        r#"REVOKE ALL ON SCHEMA "{s}" FROM "{r}";
REVOKE ALL ON ALL TABLES IN SCHEMA "{s}" FROM "{r}";
GRANT USAGE ON SCHEMA "{s}" TO "{r}";
GRANT SELECT ON ALL TABLES IN SCHEMA "{s}" TO "{r}";
"#,
        s = legacy_schema,
        r = reader
    ))
}

/// The migration writer owns DDL and data in its own schema and has no right to any other.
/// The runtime role reads and writes data but cannot create objects or change the history.
pub fn target_sql(schema: &str, writer: &str, runtime: &str) -> Result<String, String> {
    check(&[schema, writer, runtime])?;
    if writer == runtime {
        return Err("the migration writer and the runtime role must be different roles".into());
    }
    Ok(format!(
        r#"REVOKE ALL ON SCHEMA "{s}" FROM PUBLIC;
REVOKE ALL ON ALL TABLES IN SCHEMA "{s}" FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA "{s}" TO "{w}";
GRANT ALL ON ALL TABLES IN SCHEMA "{s}" TO "{w}";
GRANT ALL ON ALL SEQUENCES IN SCHEMA "{s}" TO "{w}";
GRANT USAGE ON SCHEMA "{s}" TO "{r}";
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA "{s}" TO "{r}";
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA "{s}" TO "{r}";
REVOKE INSERT, UPDATE, DELETE ON "{s}"."_ce_migration" FROM "{r}";
REVOKE INSERT, UPDATE, DELETE ON "{s}"."migration_runs", "{s}"."migration_id_map",
    "{s}"."migration_reconciliation_results", "{s}"."migration_errors",
    "{s}"."migration_artifacts" FROM "{r}";
"#,
        s = schema,
        w = writer,
        r = runtime
    ))
}

/// Fails with the list of problems when `role` can write, or create anything, in any of
/// `schemas`. A migration calls this on its legacy reader before it reads a row.
pub fn verify_reader_read_only(
    client: &mut Client,
    role: &str,
    schemas: &[&str],
) -> Result<(), Vec<String>> {
    let mut bad = Vec::new();
    for s in schemas {
        let q = client.query(
            "SELECT format('%I.%I', schemaname, tablename) FROM pg_tables WHERE schemaname = $1",
            &[s],
        );
        let tables: Vec<String> = match q {
            Ok(rows) => rows.iter().map(|r| r.get(0)).collect(),
            Err(e) => return Err(vec![format!("cannot list {s}: {e}")]),
        };
        for t in tables {
            for p in [
                "INSERT",
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
            ] {
                let has: bool = client
                    .query_one("SELECT has_table_privilege($1, $2, $3)", &[&role, &t, &p])
                    .map(|r| r.get(0))
                    .unwrap_or(true);
                if has {
                    bad.push(format!("{role} has {p} on {t}"));
                }
            }
        }
        let create: bool = client
            .query_one("SELECT has_schema_privilege($1, $2, 'CREATE')", &[&role, s])
            .map(|r| r.get(0))
            .unwrap_or(true);
        if create {
            bad.push(format!("{role} can CREATE in schema {s}"));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad)
    }
}
