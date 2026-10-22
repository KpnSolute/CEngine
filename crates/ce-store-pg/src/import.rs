//! Importing one legacy record (SOP sections 6 and 9): the entity row, its source-to-target id
//! mapping and its audit event are written in one transaction, so a crash never leaves a record
//! without its mapping or a mapping without its record. The audit event keeps the legacy actor
//! and the legacy time; the migrator is never the actor of a business act, and the audit event
//! does not name the migration run (the id map does).

use ce_store::StoreError;
use ce_store::TenantScope;
use serde_json::{json, Map, Value};

use crate::typed::{db, ident_ok, TypedPgStore};

/// Request id written on every imported audit event. It names no run.
pub const IMPORT_REQUEST_ID: &str = "legacy-import";

/// One legacy record to bring over.
#[derive(Debug, Clone)]
pub struct ImportRecord<'a> {
    pub run: &'a str,
    pub source_system: &'a str,
    pub source_table: &'a str,
    pub source_id: &'a str,
    pub source_parent_id: Option<&'a str>,
    pub entity: &'a str,
    /// The entity's fields. An `id` in here is ignored: the target id comes from the id map.
    pub data: Value,
    /// Who did it in the legacy system (a name or id as the legacy system knew it).
    pub actor: &'a str,
    /// When it happened in the legacy system, seconds since the epoch.
    pub at: i64,
    /// The state the record was in when it left the legacy system. Must be one the entity
    /// declares; the legacy history is not replayed through the transitions.
    pub state: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub target_id: String,
    /// False when the source id was already mapped; nothing was written then.
    pub created: bool,
}

impl TypedPgStore {
    pub fn migration_import(
        &self,
        scope: &TenantScope,
        rec: &ImportRecord<'_>,
    ) -> Result<Imported, StoreError> {
        if !ident_ok(&self.schema) || !ident_ok(rec.entity) {
            return Err(StoreError::BadData("invalid schema or entity name"));
        }
        let tenant = scope.tenant_id();
        let sch = &self.schema;
        let mut c = self.lock()?;
        let mut tx = c.transaction().map_err(db)?;

        // One writer per source identity: a concurrent import of the same row waits here.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!(
                "{sch}/import/{tenant}/{}/{}/{}",
                rec.source_system, rec.source_table, rec.source_id
            )],
        )
        .map_err(db)?;

        let existing = tx
            .query_opt(
                &format!(
                    r#"SELECT target_id::text, target_entity FROM "{sch}".migration_id_map
                       WHERE tenant_id = $1::text::uuid AND source_system = $2
                         AND source_table = $3 AND source_id = $4"#
                ),
                &[
                    &tenant,
                    &rec.source_system,
                    &rec.source_table,
                    &rec.source_id,
                ],
            )
            .map_err(db)?;
        if let Some(r) = existing {
            let entity: String = r.get(1);
            if entity != rec.entity {
                return Err(StoreError::Constraint(
                    "source id already maps to another entity",
                ));
            }
            return Ok(Imported {
                target_id: r.get(0),
                created: false,
            });
        }

        let id: String = tx
            .query_one("SELECT gen_random_uuid()::text", &[])
            .map_err(db)?
            .get(0);
        let mut body = rec.data.as_object().cloned().unwrap_or_default();
        body.insert("id".into(), json!(id));
        let mut record =
            self.insert_row_on(&mut tx, scope, rec.entity, Value::Object(body), None)?;
        if let Some(state) = rec.state {
            if !self.declares_state(rec.entity, state) {
                return Err(StoreError::BadData("state is not declared by the entity"));
            }
            record = self.set_state_on(&mut tx, scope, rec.entity, &id, record.version, state)?;
        }
        // The record is as old as it was in the legacy system.
        tx.execute(
            &format!(
                r#"UPDATE "{sch}"."{}" SET created_at = to_timestamp($3), updated_at = to_timestamp($3)
                   WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"#,
                rec.entity
            ),
            &[&tenant, &id, &(rec.at as f64)],
        )
        .map_err(db)?;

        tx.execute(
            &format!(
                r#"INSERT INTO "{sch}".migration_id_map
                     (tenant_id, migration_run_id, source_system, source_table, source_id,
                      target_entity, target_id, source_parent_id)
                   VALUES ($1::text::uuid, $2::text::uuid, $3, $4, $5, $6, $7::text::uuid, $8)"#
            ),
            &[
                &tenant,
                &rec.run,
                &rec.source_system,
                &rec.source_table,
                &rec.source_id,
                &rec.entity,
                &id,
                &rec.source_parent_id,
            ],
        )
        .map_err(db)?;

        if self.has_entity("audit_event") {
            let mut ev = Map::new();
            ev.insert("entity".into(), json!(rec.entity));
            ev.insert("record_id".into(), json!(id));
            ev.insert("operation".into(), json!("create"));
            ev.insert("actor".into(), json!(rec.actor));
            ev.insert("request_id".into(), json!(IMPORT_REQUEST_ID));
            ev.insert("at".into(), json!(rec.at));
            ev.insert("manifest_hash".into(), json!(self.manifest.hash));
            ev.insert("to_version".into(), json!(record.version));
            ev.insert("after".into(), record.data.clone());
            self.insert_row_on(&mut tx, scope, "audit_event", Value::Object(ev), None)?;
        }
        tx.commit().map_err(db)?;
        Ok(Imported {
            target_id: id,
            created: true,
        })
    }
}
