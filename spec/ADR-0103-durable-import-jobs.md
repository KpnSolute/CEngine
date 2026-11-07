# ADR-0103: Durable upload sessions and import jobs

Status: proposed, phase 1 implemented on branch `feat/durable-import-jobs-103`, not merged, not
deployed. Issue: KpnSolute/CEngine#103. Origin incident: MJCC-workspace#139 (invoice 511063).
Author: Claude (senior engineer review), 2026-11-07.

## 1. Context

A 13-page supplier invoice (about 474 parsed items, about 502 planned writes) cannot be posted
reliably. The browser sends a dependency-ordered series of `POST /v1/{inst}/_batch` requests.

What the code does today (verified in `ce-server/src/lib.rs`, `ce-server/src/writes.rs`,
`ce-store-pg/src/typed.rs`):

- A batch holds 1 to 100 operations and runs in one transaction. The invoice import is many
  batches, so the import as a whole is not atomic.
- Each **create** in a batch gets its own idempotency key `batch:{Idempotency-Key}:{i}`. The key row
  is inserted first, in the same transaction as the record, and stores the result. Replaying the
  same key with the same payload returns the first result.
- **Update and delete** in a batch carry no idempotency key. They are guarded only by the expected
  record version.
- The same key with a different payload returns `IdempotencyKeyReused`. A **new** key with the same
  client-chosen record id is not recognised as a retry and fails on the primary key
  (`pk_stock_line_0c57f0e3` is `PRIMARY KEY (tenant_id, id)`), which is the failure seen at
  2026-11-07 06:20 to 06:27 UTC.
- `_ce_idempotency` has no `created_at` and no expiry, so it only grows (465 `stock_line` rows at
  the last read).
- Nothing records "this upload is one job". Progress exists only in the browser.

Read-only check of `ce_mjcc_prod2` on 2026-11-07 matched the ticket: active invoice
`9949b13f-…` has 80 lines, `goods_total` 7541.72 against `stated_total` 21974.10, `reconciled =
false`, 19 new stock lines, no receipt movements, no completed `import_batch`. The deleted invoice
`cf5fce5b-…` is intact. No production row was changed.

## 2. Options

**A. Server-owned transactional job.** One request uploads and posts everything in a single
transaction. Rejected: a 500-write transaction holds locks for the whole parse, a lost response
still leaves the client guessing, and it does not help the 1,000-line or QR cases.

**B. Durable staging, then one transactional commit (chosen).** The upload becomes a job row.
Parsed lines are written to a staging table that no entity read can see. A worker leases the job,
validates, and posts invoice, lines, stock lines and receipt movements in one transaction that also
marks the job completed. Until that commit nothing posted exists, so month totals cannot be
distorted by a half-finished import.

**C. Browser-led batches with a saga (compensating deletes).** Rejected as the permanent design:
it keeps the browser as the coordinator and makes "partly posted" a visible state. It stays as the
temporary bridge (workspace #144) until B ships.

Transport for the file is a separate decision (section 6). It does not change the choice above.

## 3. Decision

Build option B as a reusable CEngine capability (`ImportJobs` in `ce-store-pg`). Keep MJCC parsing,
SKU mapping and pricing in the `mjcc-inventory` adapter. The engine owns job identity, state,
leases, staging and the audit trail. The adapter supplies a validate step and a post step.

### Job identity

- `(tenant, kind, idempotency_key)` names one job. The same key and fingerprint returns the same
  job; a different fingerprint is refused. Retrying a lost response is therefore safe.
- `(tenant, kind, scope_key)` has at most one live job (a partial unique index plus an advisory
  lock taken while creating). `scope_key` is for example `vendor/usfoods/invoice/511063`.
- Each re-upload after a job ended is a new `generation`. Generation 1 stays in history. This is
  the structural fix for the soft-delete reimport collision that workspace #143 fixed narrowly.

### State machine

```
uploaded -> parsing -> needs_review -> validated -> queued -> processing -> committing -> completed
                \-> validated                                      \-> retryable -> queued
 any non-final state except committing -> cancelled;  parsing/processing/committing/retryable -> failed
```

`completed`, `failed`, `cancelled` are final. A committing job cannot be cancelled. Transitions
are compare-and-set on the state the caller saw, and every transition is appended to
`_ce_import_job_event`.

### Leases

`claim` picks one queued job, or one whose lease expired, with `FOR UPDATE SKIP LOCKED`, and
increments `attempt`. A heartbeat extends the lease and records progress; a worker that lost its
lease gets `LeaseLost` and must stop. After `max_attempts` the job becomes `failed` with code
`attempts_exhausted` instead of looping.

### Final post (phase 2)

One transaction, guarded by an advisory lock on tenant, vendor and invoice number: insert invoice,
lines, stock lines and receipt movements from staging, update the period snapshot, check the
period and week are open, and move the job `committing -> completed` with a result summary. The
post is idempotent by job id, so a worker that dies after the commit and is reclaimed finds the job
already completed. If a single transaction proves too large in the benchmark, the fallback is
chunked posting into rows flagged pending and invisible to totals, flipped visible by one final
statement; that is decided by the benchmark, not assumed.

## 4. Classification of existing work

| Change | Class | Disposition |
| --- | --- | --- |
| Workspace #140 archive-first, delete guard | Temporary containment | Keep the delete guard until invoice void exists |
| Workspace #141 QR verify and retries | Tactical | Retire when QR runs as a job |
| Workspace #142 rollover guard | Temporary containment | Replace with a server-side idempotent rollover |
| Workspace #143 per-upload generation | Permanent for that collision | Keep; the job `generation` generalises it |
| Workspace #144 25-op batches | Temporary bridge | Retire once the adapter posts from staging |
| CEngine job store (this ADR) | Permanent architecture | Phases below |

## 5. Threat model

- **Tenant isolation:** every job, event and staging statement filters on the tenant id from the
  caller's verified scope. `claim` serves all tenants and returns the tenant on the job; the worker
  must build its scope from that, never from payload content. Tested: another tenant gets
  `NotFound`.
- **Double posting:** blocked by key dedupe, the live-scope unique index, the lease, and a post
  that is idempotent by job id. A second user cannot claim a leased job.
- **Replay with altered content:** same key with a different fingerprint is refused.
- **Privilege:** creating a job does not grant write access to entities. The post step runs with
  the actor's recorded roles and the changeset rules for governed entities (open question 3).
- **Data at rest:** staging holds parsed invoice rows, not the PDF. The PDF stays in the private
  blob/document store; no client holds service-role credentials.
- **Resource use:** staging rows are bounded by a per-job row limit, a job has one worker at a
  time, and staging for finished jobs is purged by a retention job (phase 3).

## 6. Decisions still needed from the owner

1. Job and staging tables inside CEngine (`_ce_import_*`, as implemented) versus in the adapter.
   Recommendation: CEngine, because QR generation and other tenants need the same mechanism.
2. File transport: resumable upload into `_ce_blobs` through chunked requests (recommended, no new
   dependency), or provider-native storage. `_ce_blobs` is a single `bytea` row per object today, so
   a chunk size and a maximum file size must be set.
3. Whether staged invoice import for governed entities posts directly or through a changeset
   approval (the reviewer step in `needs_review`).
4. Where the worker runs: inside `ce-serve` as a background task (recommended; Render restarts are
   covered by lease expiry) or as a separate process.
5. Retention for `_ce_idempotency`, which has no expiry today.

## 7. Plan

| Phase | Scope | State |
| --- | --- | --- |
| 1 | Job, event and staging tables; state machine; idempotent create; leases; tests | Implemented on this branch |
| 2 | HTTP API for upload session and job status; chunked file transfer; worker loop in `ce-serve`; feature flag | Not started |
| 3 | `mjcc-inventory` adapter: parse to staging, validate, atomic post; reconciliation checks | Not started |
| 4 | Benchmarks at 50, 200, 500 and 1,000 lines (throughput, p50 and p95, query count, lock time); failure-injection suite; staging run; runbook | Not started |
| 5 | QR job on the same mechanism; atomic invoice void as a dependent ticket | Not started |

Rollback: the tables are additive and unused until phase 2 is switched on. Turning the feature flag
off returns the workspace to the batch path. No migration touches an existing table.

## 8. Phase 1 results (2026-11-07)

`cargo fmt`, `cargo clippy -p ce-store-pg --all-targets -- -D warnings` and `cargo test -p
ce-store-pg` pass against Postgres 16, including 9 database tests: idempotent create and refusal
of a changed request, one live job per scope with a new generation after a re-upload, six racing
creates producing one job, compare-and-set transitions, repeatable staging closed after parsing,
four workers claiming one job, a stalled worker losing its lease, attempt exhaustion, and
completion through `committing` with tenant isolation.

Not yet covered: the failure-injection cases that need the HTTP layer (lost response after commit,
server restart mid-job), the 474-line regression, and any benchmark. Those belong to phases 2 to 4
and the issue's definition of done is not met by phase 1.

No production data was read beyond the counts in section 1, and none was written. Repairing the
partial 511063 import needs its own reviewed plan with exact target rows and a dry-run diff; this
ADR does not authorise it.
