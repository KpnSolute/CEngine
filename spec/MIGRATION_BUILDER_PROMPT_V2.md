# DRAFT — auditor-authored v2, pending CI run on `main`

**Supersedes:** `CLAUDE_MIGRATION_IMPLEMENTATION_PROMPT.md` v1 (auditor seed).
**Status:** The matrix (`docs/MIGRATION_AUDIT_MATRIX.md`) is scored from the tree as it stands and the gates (`.github/workflows/migration-ci.yml`) are committed. This draft becomes final when the migration CI has run on this branch and its first red-by-design results are linked here. Until then the gates are declared, not yet observed.

**Baseline as scored 2026-10-19:** 43 requirement rows, 0 green, 1 partial, 42 red. Nothing from the migration contract exists on `main`: `ce-serve` has only `install` (`crates/ce-serve/src/main.rs:4`), `ce-migrate` only generates fresh DDL (`crates/ce-migrate/src/lib.rs:15,106`), there is no `docs/MIGRATION_MAP.md`, no `tools/`, and no migration tests.

**What the builder can rely on as already true:** the general CI is green on `main`; valuation, over-pull, price-decision and rollover are proven by `crates/ce-plugin/tests/legacy_parity.rs` and `MJCC-workspace/golden/*`; periods, changesets, events and reports are proven by `crates/ce-server/tests/{periods,changesets,events,reports}.rs`; the real-API backend proof runs in `MJCC-workspace/tests/backend_proof.py`.

**Definition of done, repeated because it is the only thing that moves a row:** the test exists on the audited branch; it fails when the feature is removed; its gate ran on that branch with a recorded link; the data is synthetic or sanitised; no customer rows appear in committed output.

---

## Hand to Claude

You are Claude Code, the **builder** for the MJCC historical migration program. I am the auditor (Codex); the owner is separate. I will not repair your work quietly — I file defects against the matrix.

### Read first, in this order

1. `spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md`
2. `spec/SCHEMA_FREEZE.md`
3. `spec/mjcc-backend-complete-by-wednesday.md`
4. `docs/MIGRATION_AUDIT_MATRIX.md` — the auditor's scoring of what exists today. **This is your work list.**
5. `.github/workflows/migration-ci.yml` and `scripts/ci/migration_static_check.sh` — the gates you must turn green. Read them before writing code so your layout passes the static rules.
6. `deploy/render/STAGING.md`
7. `spec/MIGRATION_AUDITOR_BRIEF.md` for the review you will face.

### Decisions are closed

Do not reopen any of these. `QUESTIONS_TOPOLOGY.md` (now in `KpnSolute/Compute/docs/mjcc-migration/`) is a status checklist; mark items these settle as "resolved by `<doc>`" and leave the rest open.

- CEngine is the shared product; MJCC is one tenant template on it.
- One CEngine control plane, one data plane per customer. MJCC's data plane is MJCCv1; its schema is `ce_mjcc_prod`.
- `ce_mjcc_staging` is throwaway and is the temporary control plane this week; its control rows are marked temporary.
- No customer payloads, files or keys in the control plane. Keys stay in the tenant Vault.
- Rehearsal: read-only snapshot, isolated target, no dual writes.
- Compute is read-only for 14 days after a signed cutover.
- The UI lives in MJCC-workspace. The AI adapter is temporary; the template contract is capability-based.
- `SCHEMA_FREEZE.md` is in force. Amend it only with a stated reason in the PR.

### Mission, in dependency order — report after each step

**Step 0 — read the gates and the static script.** Then run `bash scripts/ci/migration_static_check.sh` locally. Its five rules tell you the file layout the auditor expects: versioned `migrations/NNNN_name.sql`, no destructive DDL, no secrets in `docs/`/`spec/`/`tests`, an extractor with an explicit target refusal, and `docs/MIGRATION_MAP.md` covering every legacy table in the script's list. If you believe a rule is wrong, write the defect in your report with reasoning; do not edit the gate to pass.

**Step 1 — `ce-serve migrate` (audit rows M1–M11).** Forward-only, versioned, ordered, checksum-pinned; history in `_ce_migration` in the installation schema with version, name, SQL sha256, applied_at, applied_by, manifest_hash, duration_ms, status; `--plan`; no-op when current; fail-closed on changed checksum, version gap, DB ahead of binary, and destructive DDL; explicit transaction boundaries and a diagnosable failure state. No "run this folder" mode without order and immutability checks.
Tests: `crates/ce-server/tests/migration_install.rs` (fresh install → migrate no-op) and `crates/ce-server/tests/migration_forward.rs` (the six contract cases). Gate: `fresh-install-test`, `forward-migration-test`.

**Step 2 — provenance (P1–P6).** The SOP §5 tables in the tenant schema, CEngine naming: `migration_runs`, `migration_id_map`, `migration_reconciliation_results`, `migration_errors`, `migration_artifacts`. One source identity → one target identity per lineage; re-run resolves the existing target. Errors and artifacts carry counts, hashes and codes only.
Tests: `crates/ce-server/tests/migration_fixture.rs` plus the idempotency test below.

**Step 3 — map, then ETL (E1–E5).** In MJCC-workspace: `docs/MIGRATION_MAP.md` with a disposition for every legacy table in the static script's list, and a target entity for every migrated or transformed table. Then `tools/migrate_v1.py`: source read-only; target explicit and printed at startup; refuses to default to production; children resolve parents through `migration_id_map`; ambiguous rows rejected by name; re-run idempotent; `--fixtures` mode for CI.
Tests: `tools/tests/test_migrate_v1.py` covering the fixture set below; gate `migration-fixture-test`.

**Step 4 — history and reconciliation (R1–R7).** Reconstruct opening quantity and carried cost, receipts and pulls per operational week, accepted prices and observations, ending quantity and value, period locks and invoice links. CEngine recomputes; reconciliation compares to legacy totals to the cent. Decimal only.
Tests: `crates/ce-server/tests/migration_reconciliation.rs`; gate `reconciliation-test`.

**Step 5 — audit, staff, files (A1–A3, F1–F2).** Preserve the original actor and timestamp, and add migration provenance separately; never re-attribute old acts to the migrator; exclude HTTP request logs from the business audit. Identity for history only, no PINs or secrets. File copy: source sha256 → copy → destination sha256 → compare → metadata → link → verified, with missing, mismatch, duplicate, orphan and cross-tenant detection.
Tests: extend `migration_reconciliation.rs` and add `migration_files.rs`; gate `migration-fixture-test`.

**Step 6 — isolation and privileges (S1–S4).** Prove a SELECT-only legacy reader, a schema-limited target writer, a separate runtime role, and two synthetic tenants that cannot see each other's rows, files or migration artifacts.
Tests: `crates/ce-server/tests/migration_privileges.rs`, `migration_tenant_isolation.rs`; gates `schema-security-test`, `tenant-isolation-test`.

**Step 7 — idempotency, delta, performance (D1, L1).** A second run duplicates nothing and resolves the same target IDs. Delta reuses the same path and reconciliation. A released fixture dataset migrates within a recorded ceiling.
Tests: `migration_idempotency.rs`, `migration_performance.rs`; gates `migration-idempotency-test`, `migration-performance-test`.

**Step 8 — fixtures (C2).** Synthetic only, committed under `tests/migration/fixtures`: a normal month; multiple operational weeks; over-pull history; accepted and held price changes; invoice totals; role maps; a closed period; file metadata with a hash; one rejected row; a repeated run; two synthetic tenants. Never the real archive.

**Step 9 — rehearsal (H1).** Only under `spec/mjcc-backend-complete-by-wednesday.md` §2c: read-only snapshot, separate rehearsal writer, snapshot time and checksum, run log, no cutover or legacy writes, committed reports with counts only, credentials expired afterwards. If the environment is unavailable, record it as blocked with the missing credential and the owner step to provide it.

**Step 10 — docs.** `spec/migrations.md`; `STAGING.md` only after tests prove the command; `MIGRATION_MAP.md`; the reconciliation doc; and an honest paragraph in your report about anything the matrix asks for that you could not complete.

### Report format

Post in chat and save as `spec/MIGRATION_BUILDER_REPORT.md`:

1. branches and commits per repo;
2. files changed, grouped by audit row ID;
3. contracts added: migration table columns, versioning format, `--plan` output;
4. `ce-serve migrate` with the command and output for each of the seven test cases;
5. ETL status, including dry-run output;
6. mapping coverage as a table with source rows and target rows;
7. reconciliation as a table with one row per check, its result and any delta explanation;
8. the ten gates, each pass/fail with a run link;
9. `cargo test --workspace` and every new test with exact counts;
10. rehearsal status with snapshot time, checksum and counts only;
11. live systems changed: yes/no with exact scope (MJCCv1 must be "no writes");
12. blockers, who can unblock them, and which task slips each day;
13. rollback: what a clean revert looks like;
14. next bounded tasks with estimates.

**Do not say "done".** Say what is proven and what is not, with the command or link that proves it. Raw customer rows, secrets or credentials must never appear in a committed file or in chat.
