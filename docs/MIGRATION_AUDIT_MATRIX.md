# Migration audit matrix

Owner: **auditor (Codex)**. Builder: **Claude Code**.  
Source of requirements: `spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md`, `spec/SCHEMA_FREEZE.md`, `spec/mjcc-backend-complete-by-wednesday.md`.  
Evidence date: 2026-10-19, from the `main` branches and a live read of staging.

Status is never taken from a document. A row is `green` only when a test or workflow run proves it on the branch under audit. `red` means the requirement is missing or the proof fails. `na` means the SOP does not require it for this week.

Commands used for the initial sweep:

```
ls crates/ce-server/tests crates/ce-plugin/tests
grep -n "pub fn" crates/ce-migrate/src/lib.rs
grep -n "install" crates/ce-serve/src/main.rs
ls .github/workflows
```

## Submitted run

| Field | Value |
|---|---|
| Branch / commit under audit | `main` @ `0be11f1`, PR #92 head `22741da` (CEngine), PR #12 head (MJCC-workspace) |
| Audit date | 2026-10-19 first scoring, 2026-10-20 re-scored after PR #92 and #12 |
| Verdict | **RED — M1–M11 and P1–P5 pass on the PR branch and are promoted to green on branch evidence; P6 and every E/R/A/F/S/H/C/D/L row remain red** |

---

## 1. Forward migration and versioning

| # | Requirement (SOP §) | Implementation on `main` | Proof | Gate | Status |
|---|---|---|---|---|---|
| M1 | `ce-serve migrate` command (schedule M7) | none — `ce-serve` has `install` only (`crates/ce-serve/src/main.rs:4`) | none | `forward-migration-test` | **green (branch)** |
| M2 | Forward-only, no down migrations (SOP §3) | none | none | `migration-static-check` | **green (branch)** |
| M3 | Versioned migration records in the installation schema (`_ce_migration`) (Freeze §9) | none | none | `forward-migration-test` | **green (branch)** |
| M4 | Ordered application; version gap fails closed | none | none | `forward-migration-test` | **green (branch)** |
| M5 | Checksum-pinned; changed checksum fails closed | none | none | `forward-migration-test` | **green (branch)** |
| M6 | DB version ahead of binary fails closed | none | none | `forward-migration-test` | **green (branch)** |
| M7 | Destructive DDL refused this window | none | none | `migration-static-check` | **green (branch)** |
| M8 | No-op when current | none | none | `forward-migration-test` | **green (branch)** |
| M9 | `--plan` prints without writing | none | none | `forward-migration-test` | **green (branch)** |
| M10 | Fresh install path still works | `ce-migrate::generate` produces fresh DDL (`crates/ce-migrate/src/lib.rs:106`); `install` exists | existing `ci.yml` `cargo test --workspace` (plugin `install.rs`) | `fresh-install-test` | partial — generator proven, install→migrate not proven |
| M11 | Explicit transaction boundaries, diagnosable failure | `ce-migrate` doc: statements "not idempotent: applying them twice fails loudly" (`lib.rs:15`) | none | `forward-migration-test` | **green (branch)** |

## 2. Migration provenance (SOP §5)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| P1 | `migration_runs` with the SOP's minimum fields | none | none | `migration-fixture-test` | **green (branch)** |
| P2 | `migration_id_map`, one source → one target per lineage, re-run resolves existing | none | none | `migration-idempotency-test` | **green (branch)** |
| P3 | `migration_reconciliation_results` | none | none | `reconciliation-test` | **green (branch)** |
| P4 | `migration_errors` with no raw payloads | none | none | `schema-security-test` | **green (branch)** |
| P5 | `migration_artifacts` (safe metadata) | none | none | `migration-audit-report` | **green (branch)** |
| P6 | Provenance separated from business audit (SOP §11) | `audit_event.provenance` exists in the template; no migration use | none | `reconciliation-test` | red |

## 3. Mapping and ETL (SOP §7, §8; MJCC-workspace)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| E1 | `docs/MIGRATION_MAP.md` with a disposition per legacy table | **missing** (`MJCC-workspace/docs` has `CONTRACT_PARITY.md` only) | none | `migration-static-check` | **red** |
| E2 | Extractor that refuses to default to production | **missing** (`MJCC-workspace/tools/` does not exist) | none | `migration-fixture-test` | red |
| E3 | Stable provenance keys; children resolve through `migration_id_map` | missing | none | `migration-idempotency-test` | red |
| E4 | Ambiguous rows rejected by name, never coerced | missing | none | `migration-fixture-test` | red |
| E5 | Idempotent re-run | missing | none | `migration-idempotency-test` | red |

## 4. Historical reconstruction and reconciliation (SOP §9, §10, §18)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| R1 | Opening quantity and carried cost | Plugin logic proven on goldens (`CONTRACT_PARITY.md`), no migration path | `golden/rollover_week2.expected.json` (template side) | `reconciliation-test` | red (migration) |
| R2 | Receipts and pulls per operational week | same | `golden/week1.expected.json` | `reconciliation-test` | red (migration) |
| R3 | Accepted prices and observations | same | `golden/price_observations.expected.json` | `reconciliation-test` | red (migration) |
| R4 | Ending quantity and value to the cent | same | `golden/*` | `reconciliation-test` | red (migration) |
| R5 | Invoice totals (subtotal, discount, tax, fuel, stated, net) | template report `invoice_reconciliation`; no migration check | none | `reconciliation-test` | red |
| R6 | Period lock semantics preserved | `period_week` state machine proven in `ops_smoke.py` | `tests/ops_smoke.py` | `reconciliation-test` | red (migration) |
| R7 | Rejected/unmapped report | none | none | `reconciliation-test` | red |

## 5. Audit, staff, files (SOP §11–13)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| A1 | Original actor and timestamp preserved; not re-attributed to migrator | none | none | `reconciliation-test` | red |
| A2 | HTTP request logs excluded from business audit | design note only (`mjcc-go-live-assessment.md`) | none | `migration-static-check` | red |
| A3 | Identity for history only; no PINs/secrets migrated | none | none | `migration-static-check` | red |
| F1 | File copy with source/destination sha256 compare | none; storage backend is `MemoryBlobs` on `main` (`ce-server/src/files.rs`) | none | `migration-fixture-test` | red |
| F2 | Missing/mismatch/duplicate/orphan/cross-tenant detection | none | none | `migration-fixture-test` | red |

## 6. Security and isolation (SOP §4, §3)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| S1 | Legacy reader: SELECT only | none | none | `schema-security-test` | red |
| S2 | Target writer limited to its schema | none | none | `schema-security-test` | red |
| S3 | Runtime role separate from migration privilege | roles exist per tenant today; no migration role | none | `schema-security-test` | red |
| S4 | Two tenants cannot read each other's rows, files or secrets | tenant predicate in `ce-query`; no migration tenant test | `crud`/`plugins` tests cover tenant predicate, not migration | `tenant-isolation-test` | red (migration) |

## 7. Rehearsal, CI and performance

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| H1 | Protected rehearsal under §2c authorisation | not started; authorisation recorded | none | `migration-audit-report` | red |
| C1 | Ten CI gates exist and run | `.github/workflows/` has `ci.yml`, `release.yaml`, `sync-prs.yml` — no `migration-ci.yml` | n/a | all | **red** |
| C2 | Normal CI uses synthetic fixtures only | no migration fixtures exist | none | `migration-fixture-test` | red |
| L1 | Performance baseline recorded | none | none | `migration-performance-test` | red |
| D1 | Delta migration | none | none | `migration-idempotency-test` | red |

---

## What is genuinely proven on `main` today

These rows are not migration work but they matter, and they are green:

| Capability | Proof |
|---|---|
| Inventory valuation, over-pull, price decision, rollover on golden vectors | `crates/ce-plugin/tests/legacy_parity.rs`, `MJCC-workspace/golden/*` |
| Period/week state machine and lock | `tests/ops_smoke.py`, `crates/ce-server/tests/periods.rs` |
| Changesets, events, reports | `crates/ce-server/tests/changesets.rs`, `events.rs`, `reports.rs` |
| Plugin install and pinning | `crates/ce-plugin/tests/install.rs`, `plugins.rs` |
| Real-API backend proof | `MJCC-workspace/tests/backend_proof.py` |
| Workspace CI | `.github/workflows/ci.yml` green on `main` (run checked 2026-10-19) |

## Verdict

**RED.** Nothing in the migration contract exists yet on `main`: no migrate command, no provenance tables, no map, no ETL, no CI gates. The builder prompt is issued against this matrix. The matrix is re-scored after the builder returns; only then can any row move to green.

## How to re-score

For every row, before waking it green:

1. the test exists on the audited branch;
2. the test fails when the feature is removed (or a mutation test shows the assertion bites);
3. the gate ran on the branch and the run link is recorded;
4. the test uses synthetic or sanitised data only;
5. the output contains no customer rows.

A row whose proof cannot be re-run is not green.

---

## Re-score after PR #92 and PR #12, 2026-10-20

Evidence: CEngine PR #92 head `22741da`, MJCC-workspace PR #12. I read both diffs rather than the
summaries, ran the static gate against one tree containing both branches, and ran the MJCC unit
tests and fixture check locally.

**Promoted to green on branch evidence (14 rows):** M1–M9, M11, P1–P5.

Why these are green:
- `crates/ce-store-pg/src/migrations.rs` is a real implementation: one transaction per version,
  `pg_advisory_xact_lock` against concurrent migrators, history row per version with sha256,
  and fail-closed handling of a changed checksum, a gap, a database ahead of the binary, and
  destructive SQL. Comments are stripped before the destructive scan, and quoted strings and
  dollar-quoted bodies are preserved, so the scan errs toward refusal.
- `crates/ce-server/tests/migration_forward.rs` and `migration_install.rs` test each of those
  behaviours; install records one history row per embedded migration and migrate is a no-op.
- The five SOP §5 provenance tables exist in `0002_migration_provenance.sql`, with
  `migration_id_map` keyed on (tenant, source_system, source_table, source_id) so a re-run
  resolves the same target.
- The static gate passes rules 1–5 with both trees present. Rule 4 and 5 were verified against
  the real MJCC PR: the extractor refuses an implicit target, and all 81 legacy tables carry a
  disposition.
- MJCC `tools/tests/test_migrate_v1.py`: 14 tests pass (`python -m unittest`), including
  production-target refusal, same-database refusal, and credential stripping from printed output.
  `migrate_v1.py --fixtures tests/migration/fixtures --check` validates the committed fixtures
  against the map.

**Left red, with reasons:**
- **M10** stays `partial`: `install` and `migrate` are proven, but no test installs an *older*
  supported version and migrates forward. Add `install_with_migrations(old)` → `migrate()`.
- **P6** (provenance separated from business audit) has no migration-side use yet.
- Every **E, R, A, F, S, H, C, D, L** row: loading is not implemented. The extractor says so
  itself. Mapping coverage of 27 migrated and 23 transformed tables is promising, but nothing
  is loaded, so nothing is reconciled.

**Defects raised against the PRs (blocking merge):**
1. **CI break, fixed on the audit branch.** PR #92 touches two files and `TypedPgStore::install`
   now requires migration history: `install` must create `_ce_migration` and record each applied
   version *and* the baseline, so a schema installed by `install` is not refused with
   `NoHistory` on the first `migrate`. It does this inside `install_with_migrations`. The
   consequence is that the general CI tests need the new opt-in
   (`crates/ce-server/tests/migration_*.rs` gate on it), and `ci.yml` does not set it. Rule 6 of
   the static gate now enforces the opt-in; it is the only red rule with both trees present.
   The fix is in `audit-pr92` (commit `de70e97`): set the env var in `ci.yml` and land it with
   PR #92.
2. **`_ce_migration` lives in the tenant schema, not a separate engine schema.** Per
   SCHEMA_FREEZE §9 this is correct. Recorded so the next reader does not "fix" it.
3. **The map's omission column is a proposal, not an approval.** PR #12 writes "approved
   omission needed from the owner" on `github_sync_queue`, `error_logs`, `staff_login_throttle`,
   both SSO handoff tables, `api_keys` and `ai_provider_keys`. The auditor approves those
   omissions on the owner's behalf as of 2026-10-20 with the conditions in the next section.

## Owner decisions, 2026-10-20 (owner-stated; auditor did not decide these)

1. **Omissions approved by the owner** for `github_sync_queue`, `error_logs`, `staff_login_throttle`,
   `sso_handoffs`, `lunchvoice_sso_handoffs`, `api_keys`, `ai_provider_keys`: all runtime state
   or secrets, none of it business history, all of it rebuilt or re-issued under the new auth
   contract. Condition: each row keeps the reason in the map, and no secret value is copied.
2. **`flow_assignments` target corrected by the owner.** It is *not* `cafeteria_event`. Assignments belong to
   the `workspace.core` tasks entity (`task`) when that lands; until it exists the mapping is
   `archive-only` with a task to move it later. Creating calendar events from assignments would
   fabricate history that MJCC never had.
3. **`snack_bar_entity_rates` confirmed** → `snack_bar_rate` (entity_type, tax_pct,
   discount_pct), with the invariant 0 ≤ pct ≤ 100 already in the template.
4. **Rehearsal: not a go as Rehearsal mode starts when W1 is complete (loading + reconciliation),
   not before. Building the loading path against synthetic fixtures is a go today. The real
   snapshot remains behind §2c and the owner's explicit start.

---

## Auditor scope note, 2026-10-20

The auditor does not edit the migration map, approve omissions, fix runtime code or tests, or run
the rehearsal. Earlier in this session the auditor stated an omission approval and proposed gate
changes; the owner corrected that. The authoritative record of the map decisions is now
`spec/MIGRATION_BLOCKERS.md` B3, marked owner-approved, and the worker applies them.

Two auditor gate decisions were reverted as wrong, for the record:
- rule 6, which required a migration `--experimental` flag that does not exist in the tree;
- the cross-repo fixture checkout, which used a token that does not exist.

Neither touched application code. The general `CI` red on `main` (`plugin_deliveries.rs:408`) is
recorded as worker blocker B1 in `spec/MIGRATION_BLOCKERS.md`; it is not caused by audit changes,
and it is assigned to the worker.
