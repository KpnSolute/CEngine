//! CEngine Postgres store (draft v1): the `Store` contract on PostgreSQL.
//!
//! Records live in one generic table per schema (`ce_records`, JSONB data). Typed per-entity
//! tables generated from the manifest (what the query planner reads) come with the migration
//! generator; this crate proves the contract's guarantees on a real database first:
//!
//! - **Isolation:** every statement filters on `tenant_id`, which is taken from the verified
//!   `TenantScope`, never from the caller. Primary key is `(tenant_id, entity, id)`.
//! - **Versioning:** update and delete are one `UPDATE ... WHERE version = $expected`, so two
//!   concurrent writers cannot both win.
//! - **Idempotency:** the key row is inserted first in the same transaction as the record, so a
//!   concurrent retry waits on the unique index, then replays the stored first result.
//! - **Soft delete:** `deleted_at` is set; reads skip it.
//!
//! The connection is synchronous and held behind a mutex (the typed store has a pool and TLS;
//! this generic store is the contract reference). Schema names are validated, never spliced
//! from user input.

mod blobs;
pub mod import;
pub mod loader;
pub mod migrations;
mod pool;
pub mod privileges;
pub mod provenance;
pub mod rehearsal;
mod staff;
mod tls;
mod typed;
pub use blobs::PgBlobs;
pub use staff::{PgStaff, StaffError, StaffRow};
pub use tls::{connect, Tls};
pub use typed::{PoolConfig, TypedPgStore};

use ce_store::{Record, Store, StoreError, TenantScope};
pub use postgres::types::ToSql;
pub use postgres::Client;
use postgres::NoTls;
use serde_json::{json, Value};
use std::sync::Mutex;

pub struct PgStore {
    client: Mutex<Client>,
    schema: String,
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 48
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !s.as_bytes()[0].is_ascii_digit()
}

fn db(e: postgres::Error) -> StoreError {
    if e.code().map(|c| c.code()) == Some("23505") {
        return StoreError::Constraint("unique");
    }
    StoreError::Backend(e.to_string())
}

impl PgStore {
    /// Connects and creates the schema and tables if missing.
    pub fn connect(url: &str, schema: &str) -> Result<PgStore, StoreError> {
        if !valid_ident(schema) {
            return Err(StoreError::BadData("invalid schema name"));
        }
        let mut client = Client::connect(url, NoTls).map_err(db)?;
        client
            .batch_execute(&format!(
                r#"
                CREATE SCHEMA IF NOT EXISTS "{schema}";
                CREATE TABLE IF NOT EXISTS "{schema}".ce_records (
                    tenant_id  uuid        NOT NULL,
                    entity     text        NOT NULL,
                    id         uuid        NOT NULL DEFAULT gen_random_uuid(),
                    version    bigint      NOT NULL DEFAULT 1,
                    data       jsonb       NOT NULL,
                    created_at timestamptz NOT NULL DEFAULT now(),
                    updated_at timestamptz NOT NULL DEFAULT now(),
                    deleted_at timestamptz,
                    PRIMARY KEY (tenant_id, entity, id)
                );
                CREATE TABLE IF NOT EXISTS "{schema}".ce_idempotency (
                    tenant_id uuid  NOT NULL,
                    key       text  NOT NULL,
                    entity    text  NOT NULL,
                    payload   jsonb NOT NULL,
                    result    jsonb,
                    PRIMARY KEY (tenant_id, key)
                );
                "#
            ))
            .map_err(db)?;
        Ok(PgStore {
            client: Mutex::new(client),
            schema: schema.to_string(),
        })
    }

    pub fn schema_name(&self) -> &str {
        &self.schema
    }

    /// Drops the schema and everything in it. For tests.
    pub fn drop_schema(&self) -> Result<(), StoreError> {
        let mut c = self.lock();
        c.batch_execute(&format!(
            r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#,
            self.schema
        ))
        .map_err(db)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Client> {
        self.client.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn to_record(entity: &str, id: String, version: i64, data: Value) -> Record {
    Record {
        id,
        entity: entity.to_string(),
        version: version as u64,
        data,
    }
}

fn record_json(r: &Record) -> Value {
    json!({"id": r.id, "entity": r.entity, "version": r.version, "data": r.data})
}

fn record_from_json(v: &Value) -> Option<Record> {
    Some(Record {
        id: v["id"].as_str()?.to_string(),
        entity: v["entity"].as_str()?.to_string(),
        version: v["version"].as_u64()?,
        data: v["data"].clone(),
    })
}

/// Tenant ids are canonical UUIDs (`ce-auth` checks); parse failures mean a bad id was passed
/// in, which we treat as not found rather than letting Postgres raise.
fn parse_uuid(s: &str) -> Option<uuid_text::Uuid> {
    uuid_text::Uuid::parse(s)
}

mod uuid_text {
    /// A UUID kept as validated text; bound with a `::uuid` cast so no uuid crate is needed.
    pub struct Uuid(pub String);
    impl Uuid {
        pub fn parse(s: &str) -> Option<Uuid> {
            let b = s.as_bytes();
            let ok = b.len() == 36
                && b.iter().enumerate().all(|(i, c)| match i {
                    8 | 13 | 18 | 23 => *c == b'-',
                    _ => c.is_ascii_hexdigit(),
                });
            ok.then(|| Uuid(s.to_ascii_lowercase()))
        }
    }
}

impl Store for PgStore {
    fn ping(&self) -> Result<(), StoreError> {
        let mut c = self.lock();
        c.batch_execute("SELECT 1").map_err(db)
    }

    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        let tenant = parse_uuid(scope.tenant_id())
            .ok_or(StoreError::BadData("tenant id is not a uuid"))?
            .0;
        let s = &self.schema;
        let mut c = self.lock();
        let mut tx = c.transaction().map_err(db)?;

        if let Some(key) = idempotency_key {
            let claimed = tx
                .query_opt(
                    &format!(
                        r#"INSERT INTO "{s}".ce_idempotency (tenant_id, key, entity, payload)
                           VALUES ($1::text::uuid, $2, $3, $4)
                           ON CONFLICT (tenant_id, key) DO NOTHING
                           RETURNING key"#
                    ),
                    &[&tenant, &key, &entity, &data],
                )
                .map_err(db)?;
            if claimed.is_none() {
                let row = tx
                    .query_one(
                        &format!(
                            r#"SELECT entity, payload, result FROM "{s}".ce_idempotency
                               WHERE tenant_id = $1::text::uuid AND key = $2"#
                        ),
                        &[&tenant, &key],
                    )
                    .map_err(db)?;
                let (e, payload, result): (String, Value, Option<Value>) =
                    (row.get(0), row.get(1), row.get(2));
                if e != entity || payload != data {
                    return Err(StoreError::IdempotencyKeyReused);
                }
                return result
                    .as_ref()
                    .and_then(record_from_json)
                    .ok_or(StoreError::NotFound);
            }
        }

        // A caller-chosen id (validated by the host) is stored as given; it must be unused.
        let mut data = data;
        let chosen = data
            .as_object_mut()
            .and_then(|o| o.remove("id"))
            .and_then(|v| v.as_str().map(String::from));
        let row = match &chosen {
            Some(id) => tx.query_one(
                &format!(
                    r#"INSERT INTO "{s}".ce_records (tenant_id, entity, id, data)
                       VALUES ($1::text::uuid, $2, $3::text::uuid, $4)
                       RETURNING id::text, version"#
                ),
                &[&tenant, &entity, id, &data],
            ),
            None => tx.query_one(
                &format!(
                    r#"INSERT INTO "{s}".ce_records (tenant_id, entity, data)
                       VALUES ($1::text::uuid, $2, $3)
                       RETURNING id::text, version"#
                ),
                &[&tenant, &entity, &data],
            ),
        }
        .map_err(db)?;
        let record = to_record(entity, row.get(0), row.get(1), data);
        if let Some(key) = idempotency_key {
            tx.execute(
                &format!(
                    r#"UPDATE "{s}".ce_idempotency SET result = $3
                       WHERE tenant_id = $1::text::uuid AND key = $2"#
                ),
                &[&tenant, &key, &record_json(&record)],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(record)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        let (Some(tenant), Some(id)) = (parse_uuid(scope.tenant_id()), parse_uuid(id)) else {
            return Err(StoreError::NotFound);
        };
        let s = &self.schema;
        let mut c = self.lock();
        let row = c
            .query_opt(
                &format!(
                    r#"SELECT version, data FROM "{s}".ce_records
                       WHERE tenant_id = $1::text::uuid AND entity = $2 AND id = $3::text::uuid
                         AND deleted_at IS NULL"#
                ),
                &[&tenant.0, &entity, &id.0],
            )
            .map_err(db)?
            .ok_or(StoreError::NotFound)?;
        Ok(to_record(entity, id.0, row.get(0), row.get(1)))
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        let (Some(tenant), Some(uid)) = (parse_uuid(scope.tenant_id()), parse_uuid(id)) else {
            return Err(StoreError::NotFound);
        };
        let s = &self.schema;
        let mut c = self.lock();
        let updated = c
            .query_opt(
                &format!(
                    r#"UPDATE "{s}".ce_records
                       SET data = $4, version = version + 1, updated_at = now()
                       WHERE tenant_id = $1::text::uuid AND entity = $2 AND id = $3::text::uuid
                         AND deleted_at IS NULL AND version = $5
                       RETURNING version"#
                ),
                &[
                    &tenant.0,
                    &entity,
                    &uid.0,
                    &data,
                    &(expected_version as i64),
                ],
            )
            .map_err(db)?;
        match updated {
            Some(row) => Ok(to_record(entity, uid.0, row.get(0), data)),
            None => Err(missed(&mut c, s, &tenant.0, entity, &uid.0)?),
        }
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let (Some(tenant), Some(uid)) = (parse_uuid(scope.tenant_id()), parse_uuid(id)) else {
            return Err(StoreError::NotFound);
        };
        let s = &self.schema;
        let mut c = self.lock();
        let n = c
            .execute(
                &format!(
                    r#"UPDATE "{s}".ce_records
                       SET deleted_at = now(), version = version + 1, updated_at = now()
                       WHERE tenant_id = $1::text::uuid AND entity = $2 AND id = $3::text::uuid
                         AND deleted_at IS NULL AND version = $4"#
                ),
                &[&tenant.0, &entity, &uid.0, &(expected_version as i64)],
            )
            .map_err(db)?;
        if n == 1 {
            Ok(())
        } else {
            Err(missed(&mut c, s, &tenant.0, entity, &uid.0)?)
        }
    }
}

/// After a guarded update matched nothing: was the record missing or at another version?
fn missed(
    c: &mut Client,
    schema: &str,
    tenant: &str,
    entity: &str,
    id: &str,
) -> Result<StoreError, StoreError> {
    let row = c
        .query_opt(
            &format!(
                r#"SELECT version FROM "{schema}".ce_records
                   WHERE tenant_id = $1::text::uuid AND entity = $2 AND id = $3::text::uuid
                     AND deleted_at IS NULL"#
            ),
            &[&tenant, &entity, &id],
        )
        .map_err(db)?;
    Ok(match row {
        Some(r) => StoreError::VersionConflict {
            current: r.get::<_, i64>(0) as u64,
        },
        None => StoreError::NotFound,
    })
}
