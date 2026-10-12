//! File bytes in Postgres: one `_ce_blobs` table inside the installation's schema, keyed by tenant
//! and object key. This is the persistent choice for a host with no disk of its own (a free
//! Render service); a host with real object storage supplies its own `BlobStore` instead.

use crate::pool::Pool;
use ce_store::StoreError;
use std::sync::Arc;

pub(crate) fn ddl(schema: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{schema}"."_ce_blobs" (
               tenant_id    uuid        NOT NULL,
               key          text        NOT NULL,
               content_type text        NOT NULL,
               bytes        bytea       NOT NULL,
               created_at   timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (tenant_id, key)
           )"#
    )
}

fn db(e: postgres::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Stores file bytes in the installation's schema. Cheap to clone.
#[derive(Clone)]
pub struct PgBlobs {
    pool: Arc<Pool>,
    schema: String,
}

impl PgBlobs {
    pub(crate) fn open(pool: Arc<Pool>, schema: &str) -> Result<PgBlobs, StoreError> {
        pool.get()?.batch_execute(&ddl(schema)).map_err(db)?;
        Ok(PgBlobs {
            pool,
            schema: schema.to_string(),
        })
    }

    /// Stores the bytes under the tenant and key. The same key again keeps the first copy (keys
    /// carry the content hash, so the bytes are the same).
    pub fn put(
        &self,
        tenant: &str,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), StoreError> {
        let sql = format!(
            r#"INSERT INTO "{}"."_ce_blobs" (tenant_id, key, content_type, bytes)
               VALUES ($1::text::uuid, $2, $3, $4) ON CONFLICT (tenant_id, key) DO NOTHING"#,
            self.schema
        );
        self.pool
            .get()?
            .execute(&sql, &[&tenant, &key, &content_type, &bytes])
            .map(|_| ())
            .map_err(db)
    }

    /// The bytes and content type, or `None` when there is no such object.
    pub fn get(&self, tenant: &str, key: &str) -> Result<Option<(Vec<u8>, String)>, StoreError> {
        let sql = format!(
            r#"SELECT bytes, content_type FROM "{}"."_ce_blobs" WHERE tenant_id = $1::text::uuid AND key = $2"#,
            self.schema
        );
        let rows = self.pool.get()?.query(&sql, &[&tenant, &key]).map_err(db)?;
        Ok(rows.first().map(|r| (r.get(0), r.get(1))))
    }
}
