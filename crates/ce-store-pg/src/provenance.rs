//! Writing the migration provenance tables (SOP section 5): runs, the source-to-target id map,
//! rejected rows, and reconciliation results. Counts, hashes and codes only; a rejected row's
//! payload is hashed and never stored.

use ce_store::StoreError;
use sha2::{Digest, Sha256};

use crate::typed::{db, ident_ok, TypedPgStore};

/// Longest message a rejection or a reconciliation note may carry.
pub const SAFE_MESSAGE_MAX: usize = 200;

/// What a run is about, written when it is planned.
#[derive(Debug, Clone)]
pub struct RunSpec<'a> {
    pub tenant_id: &'a str,
    pub installation_id: &'a str,
    pub source_system: &'a str,
    pub source_project_ref: &'a str,
    pub migration_version: &'a str,
    pub initiated_by: &'a str,
}

/// A source row's target, and whether this call made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub target_id: String,
    pub created: bool,
}

/// How a reconciliation check came out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    pub delta: String,
    pub pass: bool,
}

fn bad(msg: &'static str) -> StoreError {
    StoreError::BadData(msg)
}

impl TypedPgStore {
    fn prov_ok(&self) -> Result<(), StoreError> {
        if ident_ok(&self.schema) {
            Ok(())
        } else {
            Err(bad("invalid schema name"))
        }
    }

    /// Plans a run and returns its id.
    pub fn migration_start_run(&self, spec: &RunSpec<'_>) -> Result<String, StoreError> {
        self.prov_ok()?;
        let row = self
            .lock()?
            .query_one(
                &format!(
                    r#"INSERT INTO "{}".migration_runs
                         (id, tenant_id, installation_id, source_system, source_project_ref,
                          migration_version, initiated_by)
                       VALUES (gen_random_uuid(), $1::text::uuid, $2, $3, $4, $5, $6)
                       RETURNING id::text"#,
                    self.schema
                ),
                &[
                    &spec.tenant_id,
                    &spec.installation_id,
                    &spec.source_system,
                    &spec.source_project_ref,
                    &spec.migration_version,
                    &spec.initiated_by,
                ],
            )
            .map_err(db)?;
        Ok(row.get(0))
    }

    /// Moves a run along `planned -> running -> reconciling`; `passed`/`failed` come from
    /// [`migration_finish_run`](Self::migration_finish_run) only.
    pub fn migration_set_run_status(&self, run: &str, status: &str) -> Result<(), StoreError> {
        self.prov_ok()?;
        let from = match status {
            "running" => "planned",
            "reconciling" => "running",
            "cancelled" => "planned', 'running', 'reconciling",
            _ => return Err(bad("status cannot be set directly")),
        };
        let n = self
            .lock()?
            .execute(
                &format!(
                    r#"UPDATE "{}".migration_runs SET status = $2
                       WHERE id = $1::text::uuid AND status IN ('{from}')"#,
                    self.schema
                ),
                &[&run, &status],
            )
            .map_err(db)?;
        if n == 1 {
            Ok(())
        } else {
            Err(bad("run is not in a state that allows this change"))
        }
    }

    /// The target id for a source identity. A repeat returns the existing target and creates
    /// nothing, in any run. The same source id under a different target entity is refused.
    #[allow(clippy::too_many_arguments)]
    pub fn migration_map_source(
        &self,
        tenant: &str,
        run: &str,
        source_system: &str,
        source_table: &str,
        source_id: &str,
        target_entity: &str,
        source_parent_id: Option<&str>,
    ) -> Result<Mapped, StoreError> {
        self.prov_ok()?;
        let mut c = self.lock()?;
        let ins = c
            .query_opt(
                &format!(
                    r#"INSERT INTO "{}".migration_id_map
                         (tenant_id, migration_run_id, source_system, source_table, source_id,
                          target_entity, target_id, source_parent_id)
                       VALUES ($1::text::uuid, $2::text::uuid, $3, $4, $5, $6, gen_random_uuid(), $7)
                       ON CONFLICT (tenant_id, source_system, source_table, source_id) DO NOTHING
                       RETURNING target_id::text"#,
                    self.schema
                ),
                &[
                    &tenant,
                    &run,
                    &source_system,
                    &source_table,
                    &source_id,
                    &target_entity,
                    &source_parent_id,
                ],
            )
            .map_err(db)?;
        if let Some(r) = ins {
            return Ok(Mapped {
                target_id: r.get(0),
                created: true,
            });
        }
        let r = c
            .query_one(
                &format!(
                    r#"SELECT target_id::text, target_entity FROM "{}".migration_id_map
                       WHERE tenant_id = $1::text::uuid AND source_system = $2
                         AND source_table = $3 AND source_id = $4"#,
                    self.schema
                ),
                &[&tenant, &source_system, &source_table, &source_id],
            )
            .map_err(db)?;
        let entity: String = r.get(1);
        if entity != target_entity {
            return Err(StoreError::Constraint("source id already maps to another entity"));
        }
        Ok(Mapped {
            target_id: r.get(0),
            created: false,
        })
    }

    /// Records a refused row: where it came from, a code and a short message, and the sha256
    /// of its payload. The payload itself is never stored.
    #[allow(clippy::too_many_arguments)]
    pub fn migration_reject(
        &self,
        tenant: &str,
        run: &str,
        source_table: &str,
        source_id: &str,
        code: &str,
        safe_message: &str,
        payload: Option<&[u8]>,
    ) -> Result<(), StoreError> {
        self.prov_ok()?;
        if safe_message.chars().count() > SAFE_MESSAGE_MAX || code.is_empty() {
            return Err(bad("rejection message too long or code empty"));
        }
        let hash = payload.map(|p| format!("{:x}", Sha256::digest(p)));
        self.lock()?
            .execute(
                &format!(
                    r#"INSERT INTO "{}".migration_errors
                         (tenant_id, migration_run_id, source_table, source_id, error_code,
                          safe_error_message, payload_hash, severity)
                       VALUES ($1::text::uuid, $2::text::uuid, $3, $4, $5, $6, $7, 'error')"#,
                    self.schema
                ),
                &[&tenant, &run, &source_table, &source_id, &code, &safe_message, &hash],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Records one reconciliation check. `legacy` and `target` are decimal strings. It passes
    /// only when the delta is exactly zero, or when a committed `explanation` accounts for it;
    /// there is no tolerance.
    #[allow(clippy::too_many_arguments)]
    pub fn migration_check(
        &self,
        tenant: &str,
        run: &str,
        check_key: &str,
        scope_type: &str,
        scope_id: &str,
        legacy: &str,
        target: &str,
        explanation: Option<&str>,
    ) -> Result<CheckOutcome, StoreError> {
        self.prov_ok()?;
        if explanation.is_some_and(|e| e.chars().count() > SAFE_MESSAGE_MAX || e.is_empty()) {
            return Err(bad("explanation empty or too long"));
        }
        let mut c = self.lock()?;
        let d = c
            .query_one(
                "SELECT ($1::text::numeric - $2::text::numeric)::text",
                &[&legacy, &target],
            )
            .map_err(db)?;
        let delta: String = d.get(0);
        let zero = c
            .query_one("SELECT $1::text::numeric = 0", &[&delta])
            .map_err(db)?
            .get::<_, bool>(0);
        let pass = zero || explanation.is_some();
        c.execute(
            &format!(
                r#"INSERT INTO "{}".migration_reconciliation_results
                     (tenant_id, migration_run_id, check_key, scope_type, scope_id,
                      legacy_value, target_value, delta, status, details_safe)
                   VALUES ($1::text::uuid, $2::text::uuid, $3, $4, $5,
                           $6::text::numeric, $7::text::numeric, $8::text::numeric, $9, $10)"#,
                self.schema
            ),
            &[
                &tenant,
                &run,
                &check_key,
                &scope_type,
                &scope_id,
                &legacy,
                &target,
                &delta,
                &if pass { "pass" } else { "fail" },
                &explanation,
            ],
        )
        .map_err(db)?;
        Ok(CheckOutcome { delta, pass })
    }

    /// Closes a run: `passed` only if it has at least one check, no failed check and no
    /// rejected row; otherwise `failed`. Counts are taken from the tables, not from the caller.
    pub fn migration_finish_run(&self, run: &str, source_count: i64) -> Result<String, StoreError> {
        self.prov_ok()?;
        let s = &self.schema;
        let row = self
            .lock()?
            .query_one(
                &format!(
                    r#"WITH r AS (SELECT $1::text::uuid AS id),
                         t AS (SELECT count(*) AS n FROM "{s}".migration_id_map m, r WHERE m.migration_run_id = r.id),
                         e AS (SELECT count(*) AS n FROM "{s}".migration_errors m, r WHERE m.migration_run_id = r.id),
                         ok AS (SELECT count(*) AS n FROM "{s}".migration_reconciliation_results m, r
                                 WHERE m.migration_run_id = r.id AND m.status = 'pass'),
                         ko AS (SELECT count(*) AS n FROM "{s}".migration_reconciliation_results m, r
                                 WHERE m.migration_run_id = r.id AND m.status = 'fail')
                       UPDATE "{s}".migration_runs u
                          SET status = CASE WHEN ok.n > 0 AND ko.n = 0 AND e.n = 0
                                            THEN 'passed' ELSE 'failed' END,
                              completed_at = now(),
                              source_record_count = $2,
                              target_record_count = t.n,
                              rejected_record_count = e.n
                         FROM r, t, e, ok, ko
                        WHERE u.id = r.id AND u.status IN ('running', 'reconciling')
                    RETURNING u.status"#
                ),
                &[&run, &source_count],
            )
            .map_err(db);
        match row {
            Ok(r) => Ok(r.get(0)),
            Err(_) => Err(bad("run is not running or reconciling")),
        }
    }
}
