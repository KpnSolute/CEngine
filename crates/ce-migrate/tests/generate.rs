use ce_manifest::{validate, Manifest};
use ce_migrate::{generate, MigrateError};
use serde_json::json;

fn example(name: &str) -> Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    validate(std::fs::read_to_string(path).unwrap().as_bytes()).expect("example is valid")
}

fn from(v: serde_json::Value) -> Manifest {
    validate(v.to_string().as_bytes()).expect("test manifest is valid")
}

fn joined(m: &Manifest) -> String {
    generate(m, "ce_t1").unwrap().join(";\n")
}

#[test]
fn output_is_deterministic() {
    for name in ["inventory", "booking_crm", "approval_workflow"] {
        let m = example(name);
        assert_eq!(generate(&m, "ce_t1"), generate(&m, "ce_t1"), "{name}");
    }
}

#[test]
fn every_table_has_the_engine_columns() {
    let sql = joined(&example("inventory"));
    assert!(sql.starts_with("CREATE SCHEMA \"ce_t1\""));
    for col in [
        "\"id\" uuid NOT NULL",
        "\"tenant_id\" uuid NOT NULL",
        "\"version\" bigint NOT NULL DEFAULT 1",
        "\"deleted_at\" timestamptz",
    ] {
        assert_eq!(sql.matches(col).count(), 2, "{col}"); // item and period
    }
}

#[test]
fn field_types_map_to_postgres_types() {
    let sql = joined(&example("inventory"));
    assert!(sql.contains("\"name\" varchar(120) NOT NULL"));
    assert!(sql.contains("\"unit_cost\" numeric(12,4) NOT NULL"));
    assert!(sql.contains("\"period\" uuid"));
    assert!(sql.contains("CHECK (\"category\" IN ('dry', 'cold', 'frozen', 'supplies'))"));
}

#[test]
fn engine_managed_state_gets_a_checked_column() {
    let sql = joined(&example("approval_workflow"));
    assert!(sql.contains("text NOT NULL"), "{sql}");
    assert!(sql.contains("CHECK (\"state\" IN ("), "{sql}");
}

#[test]
fn references_are_tenant_scoped_foreign_keys() {
    let sql = joined(&example("inventory"));
    assert!(sql.contains(
        "FOREIGN KEY (\"tenant_id\", \"period\") REFERENCES \"ce_t1\".\"period\" (\"tenant_id\", \"id\")"
    ));
    assert!(sql.contains("PRIMARY KEY (\"tenant_id\", \"id\")"));
    assert!(!sql.contains("PRIMARY KEY (\"id\")") && !sql.contains("\"id\" uuid PRIMARY KEY"));
}

#[test]
fn indexes_and_uniques_lead_with_the_tenant() {
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { "thing": {
            "fields": {
                "code": { "type": "string", "unique": true },
                "label": { "type": "string", "indexed": true }
            },
            "sort_keys": ["label"]
        }}
    }));
    let all = generate(&m, "s").unwrap();
    for stmt in all
        .iter()
        .filter(|s| s.starts_with("CREATE UNIQUE INDEX") || s.starts_with("CREATE INDEX"))
    {
        assert!(stmt.contains("(\"tenant_id\", "), "{stmt}");
    }
    let uq = all
        .iter()
        .find(|s| s.starts_with("CREATE UNIQUE INDEX"))
        .unwrap();
    assert!(uq.ends_with("WHERE \"deleted_at\" IS NULL"), "{uq}");
    // one index for a field that is both `indexed` and a sort key
    let label = all
        .iter()
        .filter(|s| s.contains("(\"tenant_id\", \"label\""))
        .count();
    assert_eq!(label, 1);
}

#[test]
fn hard_delete_entities_have_no_deleted_at() {
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { "thing": { "soft_delete": false, "fields": { "a": { "type": "string", "unique": true } } } }
    }));
    let sql = joined(&m);
    assert!(!sql.contains("deleted_at"), "{sql}");
}

#[test]
fn engine_column_names_are_refused() {
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { "thing": { "fields": { "version": { "type": "integer" } } } }
    }));
    assert_eq!(
        generate(&m, "s"),
        Err(MigrateError::ReservedField {
            entity: "thing".into(),
            field: "version".into()
        })
    );
}

#[test]
fn bad_schema_names_are_refused() {
    let m = example("inventory");
    for bad in [
        "",
        "A",
        "1a",
        "a-b",
        "a\"; DROP SCHEMA x; --",
        &"a".repeat(64),
    ] {
        assert_eq!(generate(&m, bad), Err(MigrateError::BadSchema), "{bad}");
    }
}

#[test]
fn enum_values_are_escaped_as_literals() {
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { "thing": { "fields": { "k": { "type": "enum", "values": ["it's", "ok"] } } } }
    }));
    assert!(joined(&m).contains("IN ('it''s', 'ok')"));
}

#[test]
fn generated_names_are_short_and_do_not_collide() {
    // entity `a` field `b_c` and entity `a_b` field `c` would both read ix_a_b_c.
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": {
            "a": { "fields": { "b_c": { "type": "string", "indexed": true } } },
            "a_b": { "fields": { "c": { "type": "string", "indexed": true } } }
        }
    }));
    let names: Vec<String> = generate(&m, "s")
        .unwrap()
        .iter()
        .filter(|s| s.starts_with("CREATE INDEX"))
        .map(|s| s.split('"').nth(1).unwrap().to_string())
        .collect();
    let mut dedup = names.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), names.len(), "{names:?}");
    assert!(names.iter().all(|n| n.len() <= 63));

    let long = "x".repeat(60);
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { long.clone(): { "fields": { "y": { "type": "string", "indexed": true } } } }
    }));
    assert!(generate(&m, "s")
        .unwrap()
        .iter()
        .all(|s| s.split('"').nth(1).is_none_or(|n| n.len() <= 63)));
}

#[test]
fn string_limits_beyond_varchar_become_text_with_a_check() {
    let m = from(json!({
        "manifest_version": 1, "template": { "name": "t", "version": "0.1.0", "title": "T" },
        "entities": { "thing": { "fields": {
            "ok": { "type": "string", "max_length": 10485760 },
            "big": { "type": "string", "max_length": 10485761 }
        }}}
    }));
    let sql = joined(&m);
    assert!(sql.contains("\"ok\" varchar(10485760)"), "{sql}");
    assert!(sql.contains("\"big\" text"), "{sql}");
    assert!(sql.contains("char_length(\"big\") <= 10485761"), "{sql}");
}
