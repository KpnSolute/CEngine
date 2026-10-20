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
| Branch / commit under audit | `main` @ `e395a3a` (CEngine), `main` (MJCC-workspace) |
| Audit date | 2026-10-19 |
| Verdict | **RED — no migration implementation exists on `main`** |

---

## 1. Forward migration and versioning

| # | Requirement (SOP §) | Implementation on `main` | Proof | Gate | Status |
|---|---|---|---|---|---|
| M1 | `ce-serve migrate` command (schedule M7) | none — `ce-serve` has `install` only (`crates/ce-serve/src/main.rs:4`) | none | `forward-migration-test` | **red** |
| M2 | Forward-only, no down migrations (SOP §3) | none | none | `migration-static-check` | red |
| M3 | Versioned migration records in the installation schema (`_ce_migration`) (Freeze §9) | none | none | `forward-migration-test` | red |
| M4 | Ordered application; version gap fails closed | none | none | `forward-migration-test` | red |
| M5 | Checksum-pinned; changed checksum fails closed | none | none | `forward-migration-test` | red |
| M6 | DB version ahead of binary fails closed | none | none | `forward-migration-test` | red |
| M7 | Destructive DDL refused this window | none | none | `migration-static-check` | red |
| M8 | No-op when current | none | none | `forward-migration-test` | red |
| M9 | `--plan` prints without writing | none | none | `forward-migration-test` | red |
| M10 | Fresh install path still works | `ce-migrate::generate` produces fresh DDL (`crates/ce-migrate/src/lib.rs:106`); `install` exists | existing `ci.yml` `cargo test --workspace` (plugin `install.rs`) | `fresh-install-test` | partial — generator proven, install→migrate not proven |
| M11 | Explicit transaction boundaries, diagnosable failure | `ce-migrate` doc: statements "not idempotent: applying them twice fails loudly" (`lib.rs:15`) | none | `forward-migration-test` | red |

## 2. Migration provenance (SOP §5)

| # | Requirement | Implementation | Proof | Gate | Status |
|---|---|---|---|---|---|
| P1 | `migration_runs` with the SOP's minimum fields | none | none | `migration-fixture-test` | red |
| P2 | `migration_id_map`, one source → one target per lineage, re-run resolves existing | none | none | `migration-idempotency-test` | red |
| P3 | `migration_reconciliation_results` | none | none | `reconciliation-test` | red |
| P4 | `migration_errors` with no raw payloads | none | none | `schema-security-test` | red |
| P5 | `migration_artifacts` (safe metadata) | none | none | `migration-audit-report` | red |
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
