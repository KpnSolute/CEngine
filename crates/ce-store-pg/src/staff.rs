//! Staff accounts in Postgres: `_ce_staff` (one row per username for the installation's one
//! tenant) and `_ce_staff_event` (who changed what, never a hash or a PIN), inside the
//! installation's schema. The host maps this onto its `StaffDirectory`.

use crate::pool::Pool;
use postgres::Row;
use std::sync::Arc;

/// One account as stored. `username` is already lower-cased by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffRow {
    pub username: String,
    pub hash: String,
    pub roles: Vec<String>,
    pub active: bool,
    pub must_rotate: bool,
    pub pin_updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaffError {
    Missing,
    Exists,
    Backend,
}

pub(crate) fn ddl(schema: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{schema}"."_ce_staff" (
               tenant_id      uuid        NOT NULL,
               username       text        NOT NULL,
               pin_hash       text        NOT NULL,
               roles          text[]      NOT NULL,
               active         boolean     NOT NULL DEFAULT true,
               must_rotate    boolean     NOT NULL DEFAULT false,
               pin_updated_at timestamptz NOT NULL DEFAULT now(),
               created_at     timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (tenant_id, username)
           );
           CREATE TABLE IF NOT EXISTS "{schema}"."_ce_staff_event" (
               id        bigserial   PRIMARY KEY,
               tenant_id uuid        NOT NULL,
               at        timestamptz NOT NULL DEFAULT now(),
               actor     text        NOT NULL,
               username  text        NOT NULL,
               kind      text        NOT NULL
           )"#
    )
}

fn row(r: &Row) -> StaffRow {
    StaffRow {
        username: r.get(0),
        hash: r.get(1),
        roles: r.get(2),
        active: r.get(3),
        must_rotate: r.get(4),
        pin_updated_at: r.get::<_, i64>(5).max(0) as u64,
    }
}

const COLS: &str = "username, pin_hash, roles, active, must_rotate, \
                    extract(epoch from pin_updated_at)::bigint";

/// Staff accounts for one tenant. Cheap to clone.
#[derive(Clone)]
pub struct PgStaff {
    pool: Arc<Pool>,
    schema: String,
    tenant: String,
}

impl PgStaff {
    pub(crate) fn open(pool: Arc<Pool>, schema: &str, tenant: &str) -> Result<PgStaff, StaffError> {
        pool.get()
            .map_err(|_| StaffError::Backend)?
            .batch_execute(&ddl(schema))
            .map_err(|_| StaffError::Backend)?;
        Ok(PgStaff {
            pool,
            schema: schema.to_string(),
            tenant: tenant.to_string(),
        })
    }

    fn backend<E>(_: E) -> StaffError {
        StaffError::Backend
    }

    pub fn get(&self, username: &str) -> Result<Option<StaffRow>, StaffError> {
        let sql = format!(
            r#"SELECT {COLS} FROM "{}"."_ce_staff" WHERE tenant_id = $1::text::uuid AND username = $2"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant, &username])
            .map_err(Self::backend)?;
        Ok(rows.first().map(row))
    }

    pub fn list(&self) -> Result<Vec<StaffRow>, StaffError> {
        let sql = format!(
            r#"SELECT {COLS} FROM "{}"."_ce_staff" WHERE tenant_id = $1::text::uuid ORDER BY username"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant])
            .map_err(Self::backend)?;
        Ok(rows.iter().map(row).collect())
    }

    pub fn count(&self) -> Result<i64, StaffError> {
        let sql = format!(
            r#"SELECT count(*) FROM "{}"."_ce_staff" WHERE tenant_id = $1::text::uuid"#,
            self.schema
        );
        Ok(self
            .pool
            .get()
            .map_err(Self::backend)?
            .query_one(&sql, &[&self.tenant])
            .map_err(Self::backend)?
            .get(0))
    }

    fn event(
        tx: &mut postgres::Transaction<'_>,
        schema: &str,
        tenant: &str,
        actor: &str,
        username: &str,
        kind: &str,
    ) -> Result<(), StaffError> {
        let sql = format!(
            r#"INSERT INTO "{schema}"."_ce_staff_event" (tenant_id, actor, username, kind)
               VALUES ($1::text::uuid, $2, $3, $4)"#
        );
        tx.execute(&sql, &[&tenant, &actor, &username, &kind])
            .map(|_| ())
            .map_err(Self::backend)
    }

    pub fn create(&self, r: &StaffRow, actor: &str) -> Result<(), StaffError> {
        let mut conn = self.pool.get().map_err(Self::backend)?;
        let mut tx = conn.transaction().map_err(Self::backend)?;
        let sql = format!(
            r#"INSERT INTO "{}"."_ce_staff"
                   (tenant_id, username, pin_hash, roles, active, must_rotate, pin_updated_at)
               VALUES ($1::text::uuid, $2, $3, $4, $5, $6, to_timestamp($7::bigint))
               ON CONFLICT DO NOTHING"#,
            self.schema
        );
        let n = tx
            .execute(
                &sql,
                &[
                    &self.tenant,
                    &r.username,
                    &r.hash,
                    &r.roles,
                    &r.active,
                    &r.must_rotate,
                    &(r.pin_updated_at as i64),
                ],
            )
            .map_err(Self::backend)?;
        if n == 0 {
            return Err(StaffError::Exists);
        }
        Self::event(
            &mut tx,
            &self.schema,
            &self.tenant,
            actor,
            &r.username,
            "create",
        )?;
        tx.commit().map_err(Self::backend)
    }

    fn change(
        &self,
        set: &str,
        params: &[&(dyn postgres::types::ToSql + Sync)],
        username: &str,
        actor: &str,
        kind: &str,
    ) -> Result<(), StaffError> {
        let mut conn = self.pool.get().map_err(Self::backend)?;
        let mut tx = conn.transaction().map_err(Self::backend)?;
        let sql = format!(
            r#"UPDATE "{}"."_ce_staff" SET {set} WHERE tenant_id = $1::text::uuid AND username = $2"#,
            self.schema
        );
        let mut all: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&self.tenant, &username];
        all.extend_from_slice(params);
        if tx.execute(&sql, &all).map_err(Self::backend)? == 0 {
            return Err(StaffError::Missing);
        }
        Self::event(&mut tx, &self.schema, &self.tenant, actor, username, kind)?;
        tx.commit().map_err(Self::backend)
    }

    pub fn set_pin(
        &self,
        username: &str,
        hash: &str,
        must_rotate: bool,
        now: u64,
        actor: &str,
    ) -> Result<(), StaffError> {
        let kind = if must_rotate {
            "reset_pin"
        } else {
            "change_pin"
        };
        self.change(
            "pin_hash = $3, must_rotate = $4, pin_updated_at = to_timestamp($5::bigint)",
            &[&hash, &must_rotate, &(now as i64)],
            username,
            actor,
            kind,
        )
    }

    pub fn set_roles(
        &self,
        username: &str,
        roles: &[String],
        actor: &str,
    ) -> Result<(), StaffError> {
        self.change("roles = $3", &[&roles], username, actor, "set_roles")
    }

    pub fn set_active(&self, username: &str, active: bool, actor: &str) -> Result<(), StaffError> {
        let kind = if active { "activate" } else { "deactivate" };
        self.change("active = $3", &[&active], username, actor, kind)
    }
}
