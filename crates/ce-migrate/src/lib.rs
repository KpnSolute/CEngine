//! CEngine migration generator (draft v1). Turns a validated manifest into the DDL for a fresh
//! installation: one schema, one typed table per entity.
//!
//! Rules, all covered by tests:
//! 1. Every table has `id`, `tenant_id`, `version`, `created_at`, `updated_at` and, unless the
//!    entity opts out of soft delete, `deleted_at`. Manifests may not declare those names.
//! 2. Uniqueness and indexes always start with `tenant_id`, so one tenant's data never
//!    constrains or slows another's. Unique constraints ignore soft-deleted rows.
//! 3. References are composite `(tenant_id, id)` foreign keys, so a record can never point at
//!    another tenant's record even if application code is wrong.
//! 4. Identifiers come only from the manifest and the schema name, both re-checked here; enum
//!    values are escaped as SQL literals.
//! 5. Output is deterministic: entities and fields are sorted by name.
//!
//! The statements are for a fresh install and are not idempotent: applying them twice fails
//! loudly instead of drifting. Changing an installed manifest is a separate, later step.

use ce_manifest::Manifest;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Largest `n` Postgres accepts in `varchar(n)`.
const MAX_VARCHAR: u64 = 10_485_760;

/// Columns the engine owns; a manifest field may not use these names.
pub const RESERVED: [&str; 6] = [
    "id",
    "tenant_id",
    "version",
    "created_at",
    "updated_at",
    "deleted_at",
];

#[derive(Debug, Clone, PartialEq)]
pub enum MigrateError {
    BadSchema,
    BadName(String),
    ReservedField { entity: String, field: String },
    BadField { entity: String, field: String },
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A constraint or index name: readable prefix plus a hash of the exact parts, so two
/// different (entity, field) pairs can never produce the same name (`a`.`b_c` vs `a_b`.`c`)
/// and long names stay under Postgres's 63-byte limit.
fn name(prefix: &str, parts: &[&str]) -> String {
    let h = Sha256::digest(parts.join("\u{1f}").as_bytes());
    let tag: String = h.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let mut readable = format!("{prefix}_{}", parts.join("_"));
    readable.truncate(54);
    format!("{readable}_{tag}")
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn in_list(values: &[String]) -> String {
    values
        .iter()
        .map(|v| literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}

fn sql_type(entity: &str, name: &str, f: &Value) -> Result<String, MigrateError> {
    let bad = || MigrateError::BadField {
        entity: entity.to_string(),
        field: name.to_string(),
    };
    Ok(match f["type"].as_str().ok_or_else(bad)? {
        "string" => match f["max_length"].as_u64() {
            Some(n) if n > 0 && n <= MAX_VARCHAR => format!("varchar({n})"),
            // Longer than Postgres allows for varchar(n): text, with the limit as a CHECK below.
            _ => "text".into(),
        },
        "text" | "enum" => "text".into(),
        "integer" => "bigint".into(),
        "decimal" => {
            let p = f["precision"].as_u64().ok_or_else(bad)?;
            let s = f["scale"].as_u64().ok_or_else(bad)?;
            if p == 0 || p > 38 || s > p {
                return Err(bad());
            }
            format!("numeric({p},{s})")
        }
        "boolean" => "boolean".into(),
        "date" => "date".into(),
        "datetime" => "timestamptz".into(),
        "ref" => "uuid".into(),
        "json" | "file" => "jsonb".into(),
        _ => return Err(bad()),
    })
}

/// DDL for a fresh installation of `m` in `schema`, in the order it must run.
pub fn generate(m: &Manifest, schema: &str) -> Result<Vec<String>, MigrateError> {
    if !ident_ok(schema) {
        return Err(MigrateError::BadSchema);
    }
    let empty = serde_json::Map::new();
    let entities: BTreeMap<&String, &Value> = m.value["entities"]
        .as_object()
        .unwrap_or(&empty)
        .iter()
        .collect();

    let mut out = vec![format!("CREATE SCHEMA \"{schema}\"")];
    let mut fks = Vec::new();
    let mut indexes = Vec::new();

    for (ename, e) in &entities {
        if !ident_ok(ename) {
            return Err(MigrateError::BadName((*ename).clone()));
        }
        let soft = e["soft_delete"] != false;
        let fields: BTreeMap<&String, &Value> =
            e["fields"].as_object().unwrap_or(&empty).iter().collect();

        let mut cols = vec![
            "\"id\" uuid NOT NULL".to_string(),
            "\"tenant_id\" uuid NOT NULL".to_string(),
            "\"version\" bigint NOT NULL DEFAULT 1".to_string(),
            "\"created_at\" timestamptz NOT NULL".to_string(),
            "\"updated_at\" timestamptz NOT NULL".to_string(),
        ];
        if soft {
            cols.push("\"deleted_at\" timestamptz".to_string());
        }
        let mut checks = Vec::new();

        // An engine-managed state field is not declared in `fields`; give it a column.
        if let Some(sf) = e["state"]["field"].as_str() {
            if !fields.contains_key(&sf.to_string()) {
                if !ident_ok(sf) || RESERVED.contains(&sf) {
                    return Err(MigrateError::BadName(sf.to_string()));
                }
                let states: Vec<String> = e["state"]["states"]
                    .as_object()
                    .unwrap_or(&empty)
                    .keys()
                    .cloned()
                    .collect();
                cols.push(format!("\"{sf}\" text NOT NULL"));
                checks.push(format!(
                    "CONSTRAINT \"{}\" CHECK (\"{sf}\" IN ({}))",
                    name("ck", &[ename, sf]),
                    in_list(&states)
                ));
            }
        }

        let sort_keys: Vec<&str> = e["sort_keys"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        for (fname, f) in &fields {
            if !ident_ok(fname) {
                return Err(MigrateError::BadName((*fname).clone()));
            }
            if RESERVED.contains(&fname.as_str()) {
                return Err(MigrateError::ReservedField {
                    entity: (*ename).clone(),
                    field: (*fname).clone(),
                });
            }
            let ty = sql_type(ename, fname, f)?;
            let not_null = if f["required"] == true {
                " NOT NULL"
            } else {
                ""
            };
            cols.push(format!("\"{fname}\" {ty}{not_null}"));

            if let Some(n) = f["max_length"].as_u64() {
                if f["type"] == "string" && n > MAX_VARCHAR {
                    checks.push(format!(
                        "CONSTRAINT \"{}\" CHECK (char_length(\"{fname}\") <= {n})",
                        name("ln", &[ename, fname])
                    ));
                }
            }
            if f["type"] == "enum" {
                let values: Vec<String> = f["values"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if values.is_empty() {
                    return Err(MigrateError::BadField {
                        entity: (*ename).clone(),
                        field: (*fname).clone(),
                    });
                }
                checks.push(format!(
                    "CONSTRAINT \"{}\" CHECK (\"{fname}\" IN ({}))",
                    name("ck", &[ename, fname]),
                    in_list(&values)
                ));
            }

            if f["type"] == "ref" {
                let target = f["ref"].as_str().unwrap_or_default();
                if !entities.contains_key(&target.to_string()) {
                    return Err(MigrateError::BadField {
                        entity: (*ename).clone(),
                        field: (*fname).clone(),
                    });
                }
                fks.push(format!(
                    "ALTER TABLE \"{schema}\".\"{ename}\" ADD CONSTRAINT \"{}\" FOREIGN KEY (\"tenant_id\", \"{fname}\") REFERENCES \"{schema}\".\"{target}\" (\"tenant_id\", \"id\")",
                    name("fk", &[ename, fname])
                ));
            }

            if f["unique"] == true {
                let pred = if soft {
                    " WHERE \"deleted_at\" IS NULL"
                } else {
                    ""
                };
                indexes.push(format!(
                    "CREATE UNIQUE INDEX \"{}\" ON \"{schema}\".\"{ename}\" (\"tenant_id\", \"{fname}\"){pred}",
                    name("uq", &[ename, fname])
                ));
            }
            if f["indexed"] == true || sort_keys.contains(&fname.as_str()) {
                // Ends in `id` so keyset pagination (sort field, then id) reads in index order.
                indexes.push(format!(
                    "CREATE INDEX \"{}\" ON \"{schema}\".\"{ename}\" (\"tenant_id\", \"{fname}\", \"id\")",
                    name("ix", &[ename, fname])
                ));
            }
        }

        // Identity is (tenant, id): two tenants may hold the same id, and composite foreign
        // keys target this key.
        checks.push(format!(
            "CONSTRAINT \"{}\" PRIMARY KEY (\"tenant_id\", \"id\")",
            name("pk", &[ename])
        ));
        cols.extend(checks);
        out.push(format!(
            "CREATE TABLE \"{schema}\".\"{ename}\" (\n  {}\n)",
            cols.join(",\n  ")
        ));
        // Default list order with no sort: tenant first, then id. Hard-delete tables are
        // already served by the primary key.
        if soft {
            indexes.push(format!(
                "CREATE INDEX \"{}\" ON \"{schema}\".\"{ename}\" (\"tenant_id\", \"id\") WHERE \"deleted_at\" IS NULL",
                name("lst", &[ename])
            ));
        }
    }

    out.extend(fks);
    out.extend(indexes);
    Ok(out)
}
