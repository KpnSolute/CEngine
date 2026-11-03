//! Guest accounts in Postgres: `_ce_guest` (one row per guest of the installation's one tenant) and
//! `_ce_guest_reset` (hashes of password-reset tokens, never the tokens), inside the installation's
//! schema. The host maps this onto the server's `GuestDirectory`.

use crate::pool::Pool;
use postgres::Row;
use std::sync::Arc;

/// One account as stored. `email` is already lower-cased by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestRow {
    pub id: String,
    pub email: String,
    pub name: String,
    pub hash: String,
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestPgError {
    Missing,
    Exists,
    Backend,
}

pub(crate) fn ddl(schema: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{schema}"."_ce_guest" (
               tenant_id         uuid        NOT NULL,
               id                uuid        NOT NULL DEFAULT gen_random_uuid(),
               email             text        NOT NULL,
               name              text        NOT NULL,
               pass_hash         text        NOT NULL,
               active            boolean     NOT NULL DEFAULT true,
               sessions_ended_at timestamptz,
               created_at        timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (tenant_id, id)
           );
           CREATE UNIQUE INDEX IF NOT EXISTS "_ce_guest_email"
               ON "{schema}"."_ce_guest" (tenant_id, email);
           CREATE TABLE IF NOT EXISTS "{schema}"."_ce_guest_reset" (
               token_hash text        PRIMARY KEY,
               tenant_id  uuid        NOT NULL,
               guest_id   uuid        NOT NULL,
               expires_at timestamptz NOT NULL,
               used_at    timestamptz
           )"#
    )
}

fn row(r: &Row) -> GuestRow {
    GuestRow {
        id: r.get(0),
        email: r.get(1),
        name: r.get(2),
        hash: r.get(3),
        active: r.get(4),
    }
}

const COLS: &str = "id::text, email, name, pass_hash, active";

/// Guest accounts for one tenant. Cheap to clone.
#[derive(Clone)]
pub struct PgGuests {
    pool: Arc<Pool>,
    schema: String,
    tenant: String,
}

impl PgGuests {
    pub(crate) fn open(
        pool: Arc<Pool>,
        schema: &str,
        tenant: &str,
    ) -> Result<PgGuests, GuestPgError> {
        pool.get()
            .map_err(|_| GuestPgError::Backend)?
            .batch_execute(&ddl(schema))
            .map_err(|_| GuestPgError::Backend)?;
        Ok(PgGuests {
            pool,
            schema: schema.to_string(),
            tenant: tenant.to_string(),
        })
    }

    fn backend<E>(_: E) -> GuestPgError {
        GuestPgError::Backend
    }

    /// Creates the account, or `Exists` when the email is taken.
    pub fn create(&self, email: &str, name: &str, hash: &str) -> Result<GuestRow, GuestPgError> {
        let sql = format!(
            r#"INSERT INTO "{}"."_ce_guest" (tenant_id, email, name, pass_hash)
               VALUES ($1::text::uuid, $2, $3, $4)
               ON CONFLICT (tenant_id, email) DO NOTHING
               RETURNING {COLS}"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant, &email, &name, &hash])
            .map_err(Self::backend)?;
        rows.first().map(row).ok_or(GuestPgError::Exists)
    }

    pub fn by_email(&self, email: &str) -> Result<Option<GuestRow>, GuestPgError> {
        let sql = format!(
            r#"SELECT {COLS} FROM "{}"."_ce_guest" WHERE tenant_id = $1::text::uuid AND email = $2"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant, &email])
            .map_err(Self::backend)?;
        Ok(rows.first().map(row))
    }

    pub fn by_id(&self, id: &str) -> Result<Option<GuestRow>, GuestPgError> {
        let sql = format!(
            r#"SELECT {COLS} FROM "{}"."_ce_guest" WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant, &id])
            .map_err(Self::backend)?;
        Ok(rows.first().map(row))
    }

    /// Sets a new hash and ends every earlier session.
    pub fn set_password(&self, id: &str, hash: &str, now: u64) -> Result<(), GuestPgError> {
        let sql = format!(
            r#"UPDATE "{}"."_ce_guest" SET pass_hash = $3, sessions_ended_at = to_timestamp($4::bigint)
               WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"#,
            self.schema
        );
        let n = self
            .pool
            .get()
            .map_err(Self::backend)?
            .execute(&sql, &[&self.tenant, &id, &hash, &(now as i64)])
            .map_err(Self::backend)?;
        if n == 0 {
            Err(GuestPgError::Missing)
        } else {
            Ok(())
        }
    }

    pub fn end_sessions(&self, id: &str, now: u64) -> Result<(), GuestPgError> {
        let sql = format!(
            r#"UPDATE "{}"."_ce_guest" SET sessions_ended_at = to_timestamp($3::bigint)
               WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"#,
            self.schema
        );
        let n = self
            .pool
            .get()
            .map_err(Self::backend)?
            .execute(&sql, &[&self.tenant, &id, &(now as i64)])
            .map_err(Self::backend)?;
        if n == 0 {
            Err(GuestPgError::Missing)
        } else {
            Ok(())
        }
    }

    /// Guests whose sessions were ended at or after `since` (unix seconds), with the time.
    pub fn recently_ended(&self, since: u64) -> Result<Vec<(String, u64)>, GuestPgError> {
        let sql = format!(
            r#"SELECT id::text, extract(epoch from sessions_ended_at)::bigint FROM "{}"."_ce_guest"
               WHERE tenant_id = $1::text::uuid AND sessions_ended_at >= to_timestamp($2::bigint)"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&self.tenant, &(since as i64)])
            .map_err(Self::backend)?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1).max(0) as u64))
            .collect())
    }

    /// Keeps a reset token's hash until `expires`; also clears long-dead ones.
    pub fn put_reset(&self, id: &str, token_hash: &str, expires: u64) -> Result<(), GuestPgError> {
        let mut conn = self.pool.get().map_err(Self::backend)?;
        let mut tx = conn.transaction().map_err(Self::backend)?;
        tx.execute(
            &format!(
                r#"DELETE FROM "{}"."_ce_guest_reset" WHERE expires_at < now() - interval '1 day'"#,
                self.schema
            ),
            &[],
        )
        .map_err(Self::backend)?;
        tx.execute(
            &format!(
                r#"INSERT INTO "{}"."_ce_guest_reset" (token_hash, tenant_id, guest_id, expires_at)
                   VALUES ($1, $2::text::uuid, $3::text::uuid, to_timestamp($4::bigint))"#,
                self.schema
            ),
            &[&token_hash, &self.tenant, &id, &(expires as i64)],
        )
        .map_err(Self::backend)?;
        tx.commit().map_err(Self::backend)
    }

    /// Uses a reset token once. Returns the guest it belongs to; None when unknown, used or expired.
    pub fn take_reset(&self, token_hash: &str, now: u64) -> Result<Option<String>, GuestPgError> {
        let sql = format!(
            r#"UPDATE "{}"."_ce_guest_reset" SET used_at = now()
               WHERE token_hash = $1 AND tenant_id = $2::text::uuid AND used_at IS NULL
                 AND expires_at > to_timestamp($3::bigint)
               RETURNING guest_id::text"#,
            self.schema
        );
        let rows = self
            .pool
            .get()
            .map_err(Self::backend)?
            .query(&sql, &[&token_hash, &self.tenant, &(now as i64)])
            .map_err(Self::backend)?;
        Ok(rows.first().map(|r| r.get::<_, String>(0)))
    }
}
