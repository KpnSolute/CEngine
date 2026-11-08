//! Durable import jobs (CEngine issue #103, phase 1): the server owns a long, multi-step import
//! as one job row with a checked state machine, so a lost response, a closed tab or a restarted
//! process no longer decides whether an import finished.
//!
//! What lives here is the generic, reusable part: job rows, an append-only transition log, staged
//! rows that no entity read can see, and a lease so exactly one worker runs a job at a time. What
//! the rows mean (parsing a supplier invoice, mapping SKUs, the final ledger post) belongs to the
//! template's adapter and arrives in a later phase; nothing here writes entity records.
//!
//! - **Idempotent create:** `(tenant, kind, idempotency_key)` names one job. The same key with the
//!   same fingerprint returns that job; with a different fingerprint it is refused.
//! - **One live job per scope:** a `scope_key` (for example vendor plus invoice number) has at most
//!   one non-terminal job. A re-upload after the first job ended is a new `generation`.
//! - **Compare-and-set transitions:** a transition names the state it expects, so two writers
//!   cannot both move a job.
//! - **Leases:** `claim` hands a job to one worker with `FOR UPDATE SKIP LOCKED`; a worker that
//!   stops heartbeating loses it after the lease, and the job is reclaimed up to `max_attempts`.
//! - **Tenant isolation:** every statement except `claim` (a worker serves all tenants) filters on
//!   the tenant id given by the caller's verified scope.

use crate::pool::Pool;
use ce_store::jobs::STATES;
use ce_store::StoreError;
use postgres::error::SqlState;
use serde_json::Value;
use std::sync::Arc;

pub use ce_store::jobs::{Job, JobError, JobEvent, JobState, JobStore, NewJob};

pub(crate) fn ddl(schema: &str) -> String {
    let states = STATES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"CREATE TABLE IF NOT EXISTS "{schema}"."_ce_import_job" (
               id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
               tenant_id        uuid        NOT NULL,
               kind             text        NOT NULL,
               scope_key        text        NOT NULL,
               generation       integer     NOT NULL,
               idempotency_key  text        NOT NULL,
               fingerprint      text        NOT NULL,
               file_sha256      text,
               upload_ref       text,
               actor            text        NOT NULL,
               actor_role       text,
               state            text        NOT NULL CHECK (state IN ({states})),
               total            bigint      NOT NULL DEFAULT 0 CHECK (total >= 0),
               processed        bigint      NOT NULL DEFAULT 0 CHECK (processed >= 0),
               attempt          integer     NOT NULL DEFAULT 0,
               lease_owner      text,
               lease_expires_at timestamptz,
               error_code       text,
               error_message    text,
               result           jsonb,
               created_at       timestamptz NOT NULL DEFAULT now(),
               updated_at       timestamptz NOT NULL DEFAULT now(),
               completed_at     timestamptz,
               UNIQUE (tenant_id, kind, idempotency_key),
               UNIQUE (tenant_id, kind, scope_key, generation)
           );
           CREATE UNIQUE INDEX IF NOT EXISTS "_ce_import_job_live_scope"
               ON "{schema}"."_ce_import_job" (tenant_id, kind, scope_key)
               WHERE state NOT IN ('completed', 'failed', 'cancelled');
           CREATE INDEX IF NOT EXISTS "_ce_import_job_claim"
               ON "{schema}"."_ce_import_job" (state, lease_expires_at, created_at);
           CREATE TABLE IF NOT EXISTS "{schema}"."_ce_import_job_event" (
               id         bigserial   PRIMARY KEY,
               job_id     uuid        NOT NULL REFERENCES "{schema}"."_ce_import_job" (id),
               tenant_id  uuid        NOT NULL,
               from_state text,
               to_state   text        NOT NULL,
               detail     jsonb       NOT NULL DEFAULT '{{}}'::jsonb,
               at         timestamptz NOT NULL DEFAULT now()
           );
           CREATE INDEX IF NOT EXISTS "_ce_import_job_event_job"
               ON "{schema}"."_ce_import_job_event" (job_id, id);
           CREATE TABLE IF NOT EXISTS "{schema}"."_ce_import_staging" (
               job_id    uuid    NOT NULL REFERENCES "{schema}"."_ce_import_job" (id),
               tenant_id uuid    NOT NULL,
               seq       integer NOT NULL CHECK (seq >= 0),
               payload   jsonb   NOT NULL,
               PRIMARY KEY (job_id, seq)
           );
           ALTER TABLE "{schema}"."_ce_import_job" DROP CONSTRAINT IF EXISTS "_ce_import_job_state_check";
           ALTER TABLE "{schema}"."_ce_import_job" ADD CONSTRAINT "_ce_import_job_state_check"
               CHECK (state IN ({states}))"#
    )
}

fn db(e: postgres::Error) -> JobError {
    JobError::Store(StoreError::Backend(e.to_string()))
}

const COLS: &str = "id::text, tenant_id::text, kind, scope_key, generation, idempotency_key, \
    fingerprint, file_sha256, upload_ref, actor, state, total, processed, attempt, lease_owner, \
    (extract(epoch from lease_expires_at) * 1000)::bigint, error_code, error_message, result, \
    (extract(epoch from created_at) * 1000)::bigint, \
    (extract(epoch from updated_at) * 1000)::bigint, \
    (extract(epoch from completed_at) * 1000)::bigint, actor_role";

fn job_from(r: &postgres::Row) -> Job {
    let state: String = r.get(10);
    Job {
        id: r.get(0),
        tenant_id: r.get(1),
        kind: r.get(2),
        scope_key: r.get(3),
        generation: r.get(4),
        idempotency_key: r.get(5),
        fingerprint: r.get(6),
        file_sha256: r.get(7),
        upload_ref: r.get(8),
        actor: r.get(9),
        state: JobState::parse(&state).unwrap_or(JobState::Failed),
        total: r.get(11),
        processed: r.get(12),
        attempt: r.get(13),
        lease_owner: r.get(14),
        lease_expires_at: r.get(15),
        error_code: r.get(16),
        error_message: r.get(17),
        result: r.get(18),
        created_at: r.get(19),
        updated_at: r.get(20),
        completed_at: r.get(21),
        actor_role: r.get(22),
    }
}

/// Import jobs in the installation's schema. Cheap to clone.
#[derive(Clone)]
pub struct ImportJobs {
    pool: Arc<Pool>,
    schema: String,
}

impl ImportJobs {
    pub(crate) fn open(pool: Arc<Pool>, schema: &str) -> Result<ImportJobs, StoreError> {
        pool.get()?
            .batch_execute(&ddl(schema))
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        Ok(ImportJobs {
            pool,
            schema: schema.to_string(),
        })
    }

    fn t(&self) -> String {
        format!(r#""{}"."_ce_import_job""#, self.schema)
    }

    /// Starts a job in `uploaded`, or returns the one this key already started. The `bool` is
    /// true when a new job was made.
    pub fn create(&self, n: &NewJob<'_>) -> Result<(Job, bool), JobError> {
        let (s, t) = (&self.schema, self.t());
        let mut c = self.pool.get()?;
        let mut tx = c.transaction().map_err(db)?;
        // One creator per scope at a time, so the generation count and the live check agree.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!(
                "{s}/import_job/{}/{}/{}",
                n.tenant, n.kind, n.scope_key
            )],
        )
        .map_err(db)?;

        let prior = tx
            .query_opt(
                &format!(
                    "SELECT {COLS} FROM {t} WHERE tenant_id = $1::text::uuid AND kind = $2 \
                     AND idempotency_key = $3"
                ),
                &[&n.tenant, &n.kind, &n.idempotency_key],
            )
            .map_err(db)?;
        if let Some(r) = prior {
            let job = job_from(&r);
            if job.fingerprint != n.fingerprint || job.scope_key != n.scope_key {
                return Err(JobError::IdempotencyKeyReused);
            }
            return Ok((job, false));
        }

        let live = tx
            .query_opt(
                &format!(
                    "SELECT id::text FROM {t} WHERE tenant_id = $1::text::uuid AND kind = $2 \
                     AND scope_key = $3 AND state NOT IN ('completed', 'failed', 'cancelled')"
                ),
                &[&n.tenant, &n.kind, &n.scope_key],
            )
            .map_err(db)?;
        if let Some(r) = live {
            return Err(JobError::ScopeBusy { job_id: r.get(0) });
        }

        let generation: i32 = tx
            .query_one(
                &format!(
                    "SELECT COALESCE(MAX(generation), 0) + 1 FROM {t} \
                     WHERE tenant_id = $1::text::uuid AND kind = $2 AND scope_key = $3"
                ),
                &[&n.tenant, &n.kind, &n.scope_key],
            )
            .map_err(db)?
            .get(0);

        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO {t} (tenant_id, kind, scope_key, generation, idempotency_key, \
                     fingerprint, file_sha256, upload_ref, actor, actor_role, state, total) \
                     VALUES ($1::text::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $11, 'uploaded', $10) \
                     RETURNING {COLS}"
                ),
                &[
                    &n.tenant,
                    &n.kind,
                    &n.scope_key,
                    &generation,
                    &n.idempotency_key,
                    &n.fingerprint,
                    &n.file_sha256,
                    &n.upload_ref,
                    &n.actor,
                    &n.total,
                    &n.actor_role,
                ],
            )
            .map_err(|e| {
                if e.code() == Some(&SqlState::UNIQUE_VIOLATION) {
                    JobError::ScopeBusy {
                        job_id: String::new(),
                    }
                } else {
                    db(e)
                }
            })?;
        let job = job_from(&row);
        log_event(
            &mut tx,
            s,
            &job.id,
            n.tenant,
            None,
            "uploaded",
            &Value::Null,
        )?;
        tx.commit().map_err(db)?;
        Ok((job, true))
    }

    pub fn get(&self, tenant: &str, id: &str) -> Result<Job, JobError> {
        let t = self.t();
        let rows = self
            .pool
            .get()?
            .query(
                &format!(
                    "SELECT {COLS} FROM {t} WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"
                ),
                &[&tenant, &id],
            )
            .map_err(db)?;
        rows.first().map(job_from).ok_or(JobError::NotFound)
    }

    /// The job's transition log, oldest first.
    pub fn events(&self, tenant: &str, id: &str) -> Result<Vec<JobEvent>, JobError> {
        let s = &self.schema;
        let rows = self
            .pool
            .get()?
            .query(
                &format!(
                    r#"SELECT from_state, to_state, detail,
                              (extract(epoch from at) * 1000)::bigint
                       FROM "{s}"."_ce_import_job_event"
                       WHERE tenant_id = $1::text::uuid AND job_id = $2::text::uuid ORDER BY id"#
                ),
                &[&tenant, &id],
            )
            .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| JobEvent {
                from_state: r.get(0),
                to_state: r.get(1),
                detail: r.get(2),
                at: r.get(3),
            })
            .collect())
    }

    /// The tenant's jobs, newest first, at most `limit` (1 to 500); only `actor`'s when given.
    pub fn list(
        &self,
        tenant: &str,
        actor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Job>, JobError> {
        let t = self.t();
        let limit = limit.clamp(1, 500);
        let rows = self
            .pool
            .get()?
            .query(
                &format!(
                    "SELECT {COLS} FROM {t} WHERE tenant_id = $1::text::uuid \
                     AND ($2::text IS NULL OR actor = $2) ORDER BY created_at DESC, id LIMIT $3"
                ),
                &[&tenant, &actor, &limit],
            )
            .map_err(db)?;
        Ok(rows.iter().map(job_from).collect())
    }

    /// Starts the next generation of a failed or cancelled job with a copy of its staged rows.
    /// The new job is `uploaded`; submitting it rehearses and queues it as usual. The old job and
    /// its history stay as they are. A repeat returns the generation this restart already made.
    pub fn restart(&self, tenant: &str, id: &str, by: &str) -> Result<Job, JobError> {
        let (s, t) = (&self.schema, self.t());
        let old = self.get(tenant, id)?;
        if !matches!(old.state, JobState::Failed | JobState::Cancelled) {
            return Err(JobError::StateChanged { actual: old.state });
        }
        let key = format!("restart:{}", old.id);
        let mut c = self.pool.get()?;
        let mut tx = c.transaction().map_err(db)?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!(
                "{s}/import_job/{}/{}/{}",
                tenant, old.kind, old.scope_key
            )],
        )
        .map_err(db)?;
        if let Some(r) = tx
            .query_opt(
                &format!(
                    "SELECT {COLS} FROM {t} WHERE tenant_id = $1::text::uuid AND kind = $2 \
                     AND idempotency_key = $3"
                ),
                &[&tenant, &old.kind, &key],
            )
            .map_err(db)?
        {
            return Ok(job_from(&r));
        }
        if let Some(r) = tx
            .query_opt(
                &format!(
                    "SELECT id::text FROM {t} WHERE tenant_id = $1::text::uuid AND kind = $2 \
                     AND scope_key = $3 AND state NOT IN ('completed', 'failed', 'cancelled')"
                ),
                &[&tenant, &old.kind, &old.scope_key],
            )
            .map_err(db)?
        {
            return Err(JobError::ScopeBusy { job_id: r.get(0) });
        }
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO {t} (tenant_id, kind, scope_key, generation, idempotency_key, \
                     fingerprint, file_sha256, upload_ref, actor, actor_role, state, total) \
                     SELECT tenant_id, kind, scope_key, \
                       (SELECT COALESCE(MAX(generation), 0) + 1 FROM {t} g \
                        WHERE g.tenant_id = o.tenant_id AND g.kind = o.kind AND g.scope_key = o.scope_key), \
                       $3, fingerprint, file_sha256, upload_ref, actor, actor_role, 'uploaded', total \
                     FROM {t} o WHERE o.tenant_id = $1::text::uuid AND o.id = $2::text::uuid \
                     RETURNING {COLS}"
                ),
                &[&tenant, &id, &key],
            )
            .map_err(db)?;
        let job = job_from(&row);
        tx.execute(
            &format!(
                r#"INSERT INTO "{s}"."_ce_import_staging" (job_id, tenant_id, seq, payload)
                   SELECT $1::text::uuid, tenant_id, seq, payload FROM "{s}"."_ce_import_staging"
                   WHERE tenant_id = $2::text::uuid AND job_id = $3::text::uuid"#
            ),
            &[&job.id, &tenant, &id],
        )
        .map_err(db)?;
        let detail = serde_json::json!({"restart_of": old.id, "by": by});
        log_event(&mut tx, s, &job.id, tenant, None, "uploaded", &detail)?;
        tx.commit().map_err(db)?;
        Ok(job)
    }

    /// Moves a job from the state the caller saw to the next one. `error` is a code and message
    /// kept on the job (used for `Retryable` and `Failed`).
    pub fn transition(
        &self,
        tenant: &str,
        id: &str,
        from: JobState,
        to: JobState,
        error: Option<(&str, &str)>,
    ) -> Result<Job, JobError> {
        if !from.can_move_to(to) {
            return Err(JobError::BadTransition { from, to });
        }
        let (s, t) = (&self.schema, self.t());
        let (code, msg) = error.map_or((None, None), |(c, m)| (Some(c), Some(m)));
        let live = matches!(to, JobState::Processing | JobState::Committing);
        let mut c = self.pool.get()?;
        let mut tx = c.transaction().map_err(db)?;
        let row = tx
            .query_opt(
                &format!(
                    "UPDATE {t} SET state = $4, updated_at = now(), \
                     completed_at = CASE WHEN $5 THEN now() ELSE completed_at END, \
                     lease_owner = CASE WHEN $6 THEN lease_owner ELSE NULL END, \
                     lease_expires_at = CASE WHEN $6 THEN lease_expires_at ELSE NULL END, \
                     error_code = COALESCE($7, error_code), error_message = COALESCE($8, error_message) \
                     WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid AND state = $3 \
                     RETURNING {COLS}"
                ),
                &[
                    &tenant,
                    &id,
                    &from.as_str(),
                    &to.as_str(),
                    &to.is_terminal(),
                    &live,
                    &code,
                    &msg,
                ],
            )
            .map_err(db)?;
        let Some(row) = row else {
            return Err(self.missing_or_changed(&mut tx, tenant, id));
        };
        let job = job_from(&row);
        let detail = code.map_or(Value::Null, |c| serde_json::json!({"error_code": c}));
        log_event(
            &mut tx,
            s,
            id,
            tenant,
            Some(from.as_str()),
            to.as_str(),
            &detail,
        )?;
        tx.commit().map_err(db)?;
        Ok(job)
    }

    fn missing_or_changed(
        &self,
        c: &mut impl postgres::GenericClient,
        tenant: &str,
        id: &str,
    ) -> JobError {
        let t = self.t();
        match c.query_opt(
            &format!(
                "SELECT state FROM {t} WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid"
            ),
            &[&tenant, &id],
        ) {
            Ok(Some(r)) => {
                let st: String = r.get(0);
                JobState::parse(&st).map_or(JobError::NotFound, |actual| JobError::StateChanged {
                    actual,
                })
            }
            Ok(None) => JobError::NotFound,
            Err(e) => db(e),
        }
    }

    /// Finishes a committing job and records its result. Phase 2 does this inside the posting
    /// transaction; this form is for adapters that post elsewhere.
    pub fn complete(&self, tenant: &str, id: &str, result: &Value) -> Result<Job, JobError> {
        let (s, t) = (&self.schema, self.t());
        let mut c = self.pool.get()?;
        let mut tx = c.transaction().map_err(db)?;
        let row = tx
            .query_opt(
                &format!(
                    "UPDATE {t} SET state = 'completed', result = $3, processed = total, \
                     updated_at = now(), completed_at = now(), lease_owner = NULL, \
                     lease_expires_at = NULL \
                     WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid \
                     AND state = 'committing' RETURNING {COLS}"
                ),
                &[&tenant, &id, result],
            )
            .map_err(db)?;
        let Some(row) = row else {
            return Err(self.missing_or_changed(&mut tx, tenant, id));
        };
        let job = job_from(&row);
        log_event(
            &mut tx,
            s,
            id,
            tenant,
            Some("committing"),
            "completed",
            &Value::Null,
        )?;
        tx.commit().map_err(db)?;
        Ok(job)
    }

    /// Hands the oldest runnable job to `worker` for `lease_secs`: a queued job, or one whose
    /// worker stopped heartbeating. A job that has used `max_attempts` is failed instead and the
    /// next one is tried. Serves every tenant.
    pub fn claim(
        &self,
        worker: &str,
        lease_secs: i64,
        max_attempts: i32,
    ) -> Result<Option<Job>, JobError> {
        let (s, t) = (&self.schema, self.t());
        loop {
            let mut c = self.pool.get()?;
            let mut tx = c.transaction().map_err(db)?;
            let row = tx
                .query_opt(
                    &format!(
                        "SELECT id::text, tenant_id::text, state, attempt FROM {t} \
                         WHERE state = 'queued' \
                            OR (state IN ('processing', 'committing') AND lease_expires_at < now()) \
                         ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED"
                    ),
                    &[],
                )
                .map_err(db)?;
            let Some(row) = row else {
                return Ok(None);
            };
            let (id, tenant, was, attempt): (String, String, String, i32) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            if attempt >= max_attempts {
                tx.execute(
                    &format!(
                        "UPDATE {t} SET state = 'failed', error_code = 'attempts_exhausted', \
                         error_message = 'the job used all of its attempts', updated_at = now(), \
                         completed_at = now(), lease_owner = NULL, lease_expires_at = NULL \
                         WHERE id = $1::text::uuid"
                    ),
                    &[&id],
                )
                .map_err(db)?;
                let detail = serde_json::json!({"error_code": "attempts_exhausted"});
                log_event(&mut tx, s, &id, &tenant, Some(&was), "failed", &detail)?;
                tx.commit().map_err(db)?;
                continue;
            }
            let claimed = tx
                .query_one(
                    &format!(
                        "UPDATE {t} SET state = CASE WHEN state = 'committing' THEN 'committing' ELSE 'processing' END, \
                         attempt = attempt + 1, lease_owner = $2, \
                         lease_expires_at = now() + make_interval(secs => $3), updated_at = now() \
                         WHERE id = $1::text::uuid RETURNING {COLS}"
                    ),
                    &[&id, &worker, &(lease_secs as f64)],
                )
                .map_err(db)?;
            let job = job_from(&claimed);
            let detail = serde_json::json!({"worker": worker, "attempt": job.attempt});
            log_event(
                &mut tx,
                s,
                &id,
                &tenant,
                Some(&was),
                job.state.as_str(),
                &detail,
            )?;
            tx.commit().map_err(db)?;
            return Ok(Some(job));
        }
    }

    /// Extends the worker's lease and optionally records progress. Fails with `LeaseLost` when the
    /// lease ran out or went to another worker, so a stalled worker stops instead of doubling up.
    pub fn heartbeat(
        &self,
        tenant: &str,
        id: &str,
        worker: &str,
        lease_secs: i64,
        processed: Option<i64>,
    ) -> Result<Job, JobError> {
        let t = self.t();
        let row = self
            .pool
            .get()?
            .query_opt(
                &format!(
                    "UPDATE {t} SET lease_expires_at = now() + make_interval(secs => $4), \
                     processed = COALESCE($5, processed), updated_at = now() \
                     WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid \
                     AND state IN ('processing', 'committing') AND lease_owner = $3 \
                     AND lease_expires_at > now() RETURNING {COLS}"
                ),
                &[&tenant, &id, &worker, &(lease_secs as f64), &processed],
            )
            .map_err(db)?;
        row.as_ref().map(job_from).ok_or(JobError::LeaseLost)
    }

    /// Stores parsed rows against the job. A row already staged at the same position is kept, so
    /// the same call can be repeated; only a job still being uploaded or parsed accepts rows.
    /// Returns how many rows were new.
    pub fn stage_rows(
        &self,
        tenant: &str,
        id: &str,
        rows: &[(i32, Value)],
    ) -> Result<u64, JobError> {
        let (s, t) = (&self.schema, self.t());
        let mut c = self.pool.get()?;
        let mut tx = c.transaction().map_err(db)?;
        let open = tx
            .query_opt(
                &format!(
                    "SELECT 1 FROM {t} WHERE tenant_id = $1::text::uuid AND id = $2::text::uuid \
                     AND state IN ('uploaded', 'parsing') FOR UPDATE"
                ),
                &[&tenant, &id],
            )
            .map_err(db)?;
        if open.is_none() {
            return Err(self.missing_or_changed(&mut tx, tenant, id));
        }
        let mut added = 0;
        for (seq, payload) in rows {
            let new = tx
                .execute(
                    &format!(
                        r#"INSERT INTO "{s}"."_ce_import_staging" (job_id, tenant_id, seq, payload)
                           VALUES ($1::text::uuid, $2::text::uuid, $3, $4)
                           ON CONFLICT (job_id, seq) DO NOTHING"#
                    ),
                    &[&id, &tenant, seq, payload],
                )
                .map_err(db)?;
            if new == 0 {
                // A repeat must carry the same content; anything else is a client bug that would
                // otherwise be silently dropped.
                let same: bool = tx
                    .query_one(
                        &format!(
                            r#"SELECT payload = $3 FROM "{s}"."_ce_import_staging"
                               WHERE job_id = $1::text::uuid AND seq = $2"#
                        ),
                        &[&id, seq, payload],
                    )
                    .map_err(db)?
                    .get(0);
                if !same {
                    return Err(JobError::StagedMismatch { seq: *seq });
                }
            }
            added += new;
        }
        tx.commit().map_err(db)?;
        Ok(added)
    }

    /// Staged rows with `seq` above `after`, in order, at most `limit`.
    pub fn staged(
        &self,
        tenant: &str,
        id: &str,
        after: i32,
        limit: i64,
    ) -> Result<Vec<(i32, Value)>, JobError> {
        let s = &self.schema;
        let rows = self
            .pool
            .get()?
            .query(
                &format!(
                    r#"SELECT seq, payload FROM "{s}"."_ce_import_staging"
                       WHERE tenant_id = $1::text::uuid AND job_id = $2::text::uuid AND seq > $3
                       ORDER BY seq LIMIT $4"#
                ),
                &[&tenant, &id, &after, &limit],
            )
            .map_err(db)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// How many rows are staged for the job.
    pub fn staged_count(&self, tenant: &str, id: &str) -> Result<i64, JobError> {
        let s = &self.schema;
        Ok(self
            .pool
            .get()?
            .query_one(
                &format!(
                    r#"SELECT count(*) FROM "{s}"."_ce_import_staging"
                       WHERE tenant_id = $1::text::uuid AND job_id = $2::text::uuid"#
                ),
                &[&tenant, &id],
            )
            .map_err(db)?
            .get(0))
    }
}

fn log_event(
    tx: &mut postgres::Transaction<'_>,
    schema: &str,
    id: &str,
    tenant: &str,
    from: Option<&str>,
    to: &str,
    detail: &Value,
) -> Result<(), JobError> {
    let detail = if detail.is_null() {
        serde_json::json!({})
    } else {
        detail.clone()
    };
    tx.execute(
        &format!(
            r#"INSERT INTO "{schema}"."_ce_import_job_event"
               (job_id, tenant_id, from_state, to_state, detail)
               VALUES ($1::text::uuid, $2::text::uuid, $3, $4, $5)"#
        ),
        &[&id, &tenant, &from, &to, &detail],
    )
    .map_err(db)?;
    Ok(())
}

impl JobStore for ImportJobs {
    fn create(&self, n: &NewJob<'_>) -> Result<(Job, bool), JobError> {
        ImportJobs::create(self, n)
    }
    fn get(&self, tenant: &str, id: &str) -> Result<Job, JobError> {
        ImportJobs::get(self, tenant, id)
    }
    fn events(&self, tenant: &str, id: &str) -> Result<Vec<JobEvent>, JobError> {
        ImportJobs::events(self, tenant, id)
    }
    fn transition(
        &self,
        tenant: &str,
        id: &str,
        from: JobState,
        to: JobState,
        error: Option<(&str, &str)>,
    ) -> Result<Job, JobError> {
        ImportJobs::transition(self, tenant, id, from, to, error)
    }
    fn complete(&self, tenant: &str, id: &str, result: &Value) -> Result<Job, JobError> {
        ImportJobs::complete(self, tenant, id, result)
    }
    fn claim(
        &self,
        worker: &str,
        lease_secs: i64,
        max_attempts: i32,
    ) -> Result<Option<Job>, JobError> {
        ImportJobs::claim(self, worker, lease_secs, max_attempts)
    }
    fn heartbeat(
        &self,
        tenant: &str,
        id: &str,
        worker: &str,
        lease_secs: i64,
        processed: Option<i64>,
    ) -> Result<Job, JobError> {
        ImportJobs::heartbeat(self, tenant, id, worker, lease_secs, processed)
    }
    fn stage_rows(&self, tenant: &str, id: &str, rows: &[(i32, Value)]) -> Result<u64, JobError> {
        ImportJobs::stage_rows(self, tenant, id, rows)
    }
    fn staged(
        &self,
        tenant: &str,
        id: &str,
        after: i32,
        limit: i64,
    ) -> Result<Vec<(i32, Value)>, JobError> {
        ImportJobs::staged(self, tenant, id, after, limit)
    }
    fn staged_count(&self, tenant: &str, id: &str) -> Result<i64, JobError> {
        ImportJobs::staged_count(self, tenant, id)
    }
    fn list(&self, tenant: &str, actor: Option<&str>, limit: i64) -> Result<Vec<Job>, JobError> {
        ImportJobs::list(self, tenant, actor, limit)
    }
    fn restart(&self, tenant: &str, id: &str, by: &str) -> Result<Job, JobError> {
        ImportJobs::restart(self, tenant, id, by)
    }
}
