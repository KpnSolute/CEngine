# Claude Code handoff: MJCC migration — gaps G1–G13

**Status:** FINAL, auditor-authored, 2026-10-20. Replaces the auditor seed of 2026-10-19 and
supersedes `MIGRATION_BUILDER_PROMPT_V2.md` and `MIGRATION_BUILDER_PROMPT_V3.md`.
**Builder:** Claude Code. **Auditor:** Codex. **Owner:** the principal.
This prompt is the worker's authority for what to do; it carries no production or cutover authority.

The red gaps below are the auditor's finding from the tree, the two open PRs and CI. Each one names
the test or gate that must prove it. Work them in order: G1 first because it blocks every CI signal.

---

## 0. Read first

1. `spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md` — the contract.
2. `spec/SCHEMA_FREEZE.md` — schema, provenance, idempotency and auth decisions.
3. `spec/mjcc-backend-complete-by-wednesday.md` — schedule and ground rules.
4. `docs/MIGRATION_AUDIT_MATRIX.md` — what is red and why, grouped G1–G13.
5. `spec/MIGRATION_BLOCKERS.md` — the blocker register.
6. `spec/MIGRATION_REHEARSAL_GATE.md` — what unlocks the real-data rehearsal.
7. `MJCC-workspace/docs/MIGRATION_MAP.md` — the map you will update.

`docs/QUESTIONS_TOPOLOGY.md` is a checklist only, now at
`KpnSolute/Compute/docs/mjcc-migration/`. Mark decided entries "resolved by `<doc>`"; do not wait
on it.

## 1. Closed decisions

- CEngine is the shared runtime; MJCC is one tenant template.
- Control plane separate from tenant data planes. MJCC's data plane is MJCCv1; production schema
  `ce_mjcc_prod`. `ce_mjcc_staging` is throwaway and is the temporary control plane this week.
- Full write only to staging. **Never write MJCCv1.**
- No customer payloads, files or secrets in the control plane.
- Rehearsal: read-only snapshot, isolated target, blocked until §6 is satisfied.
- One writer at cutover; Compute read-only for 14 days.
- UI in MJCC-workspace; AI adapter temporary; template contract capability-based.

## 2. G1 — runtime blocker (do this first)

**Test:** `crates/ce-server/tests/plugin_deliveries.rs:408`,
`jobs_waiting_for_a_retry_never_hide_a_due_one`, `assert_eq!(done, 1)` → left 0.
**Evidence:** CI run `39369255125`, job `rust`, commit `74eb88d`, failed; re-run failed again at
`09:46:25Z`, same test, same assertion. Reproducible twice.

```text
thread 'jobs_waiting_for_a_retry_never_hide_a_due_one' panicked at crates/ce-server/tests/plugin_deliveries.rs:408:5:
assertion `left == right` failed
  left: 0
 right: 1
```

The relay lists `status in (queued, in_flight)` with `sort: []` in `crates/ce-server/src/deliveries.rs`,
so it pages a changing row set in whatever order Postgres returns. The due job is created before
120 waiters. `BATCH = 50`, `MAX_PAGES = 20`, so page starvation is not the explanation; ordering is
the suspect.

Required:
1. Reproduce locally, report pass/fail over five runs.
2. Confirm or refute the ordering hypothesis with page logs (page sizes, the page the due job lands
   on). If a deterministic sort (`created_at, id`) makes it deterministic, say so with the run.
3. Fix the cause. Do **not** raise `MAX_PAGES`, shrink the fixture, or add sleeps.
4. Fix the failure path: a failed assertion currently panics inside the runtime and the cleanup then
   aborts with `Cannot start a runtime from within a runtime` (SIGABRT), hiding other results.
5. Prove it: five clean local runs plus a green `rust` job link on your branch.

## 3. G3 — apply the owner-approved map (eight rows)

Owner-approved: **2026-10-20**. Apply exactly in `MJCC-workspace/docs/MIGRATION_MAP.md`:
`github_sync_queue`, `error_logs`, `staff_login_throttle`, `sso_handoffs`,
`lunchvoice_sso_handoffs`, `api_keys`, `ai_provider_keys` → **omitted**, each with the note
`owner-approved: 2026-10-20` and its one-line reason (transient runtime, auth, integration, log or
secret state; rebuilt, reissued or regenerated).

`flow_assignments` → **archive-only**. Remove the `cafeteria_event` target; it must not become
`cafeteria_event` or another operational entity, because that would invent historical meaning the
source never had.

`snack_bar_entity_rates → snack_bar_rate` stays unchanged.

Then: `migrate_v1.py --check` passes, static gate rule 5 passes, and no test asserts the old
disposition.

## 4. G4/G11 — make the gates measure something (E2–E5, C1)

Five DB-backed gates currently fail with "no test target named …". Create the files with real
tests, or the gate cannot mean anything:

| Gate | Test file to create |
|---|---|
| `migration-fixture-test` | `MJCC-workspace/tools/tests/` (extend) |
| `reconciliation-test` | `crates/ce-server/tests/migration_reconciliation.rs` |
| `migration-idempotency-test` | `crates/ce-server/tests/migration_idempotency.rs` |
| `tenant-isolation-test` | `crates/ce-server/tests/migration_tenant_isolation.rs` |
| `schema-security-test` | `crates/ce-server/tests/migration_privileges.rs` |
| `migration-performance-test` | `crates/ce-server/tests/migration_performance.rs` |

Names are the gates' contract; do not rename the gates. A test that skips when no database is set
is fine; a test that asserts nothing is not.

## 5. G12 — the fixture contract (C2)

State in `spec/migrations.md` the exact command the CEngine fixture gate should run without a
database, given the fixtures and map live in MJCC-workspace. Choose:
- (a) `migrate_v1.py --plan-only --fixtures DIR --map FILE`, no network, no database; or
- (b) a scripted export of fixtures and map into CEngine `tests/migration/`, with the MJCC copy as
  source of truth.
Give me the command line. I write the gate to your contract; you do not edit the gate.

Also answer in one paragraph: where the **control plane's** own migration history lives when the
control plane is migrated (Freeze §9). A decision, not implementation, is enough this week.

## 6. G5/G6/G7/G8 — loading, reconciliation, audit, files (E, R, A, F, S)

Implement in this order, each with a test that fails when the code is removed:

1. **Loader skeleton.** Open the target; write `migration_runs` `planned` → `running`; reuse the
   extractor's production and same-database refusals rather than duplicating them.
2. **Parents, then children.** Parents first (`location`, `inventory_category`, `vendor`,
   `staff_member`, `period`, `period_week`, `stock_item` + `stock_item_detail`); children resolve
   through `migration_id_map`. Every entity write and its id-map row are in the same transaction.
3. **History, not snapshots.** Opening quantity and carried cost, receipts and pulls per week,
   accepted prices and observations, ending quantity and value, period locks, invoice links. CEngine
   recomputes; decimal only.
4. **Reconciliation.** One `migration_reconciliation_results` row per SOP §18 check, with
   `legacy_value`, `target_value`, `delta`, and `pass` only when the delta is zero or explained in a
   committed note. "Close enough" fails.
5. **Rejected rows.** Every refusal writes `migration_errors` with source table, source id, code and
   a safe message — never the payload.
6. **Idempotency.** Second run: same target ids, zero new rows.
7. **Audit preservation (G6).** Original actor and timestamp survive; the migrator is never the
   actor of a business act; request logs are excluded from business audit.
8. **Files (G7).** Copy with source sha256 → copy → destination sha256 → compare → metadata → link →
   verified. Detect missing, mismatch, duplicate, orphan and cross-tenant. The `main` blob backend is
   in-memory; if you need the S3 store first, say so and order it.
9. **Privileges and isolation (G8).** Prove a SELECT-only legacy reader, a schema-limited writer, a
   runtime role separate from migration, and two synthetic tenants that cannot see each other's rows,
   files or migration artifacts.

## 7. G13 — M10 and P6

- **M10:** `install_with_migrations(old)` → `migrate(&embedded())` → history shows the forward step,
  checksums correct, `plan.current == 2`.
- **P6:** a migrated record's `audit_event` keeps the original actor and timestamp, while the
  migration is recorded only in `migration_runs`/`migration_artifacts`.

## 8. G9/G10 — do not start these

Rehearsal (G9) and performance/delta (G10) wait until section 6 passes on fixtures. The rehearsal
gate is `spec/MIGRATION_REHEARSAL_GATE.md`: eight prerequisites, a snapshot with time and sha256, a
rehearsal-only writer, and the owner's explicit start. **Do not run against real data. Do not touch
MJCCv1.**

## 9. Report

Save as `spec/MIGRATION_BUILDER_REPORT.md` and post in chat:

1. branches and commits per repo;
2. G1: five local runs with pass/fail, the ordering evidence, the fix, the CI link;
3. G3: the eight rows before and after;
4. G12: the fixture command you want gated, and the control-plane history answer;
5. G13: the two tests with output;
6. G5–G8: per table written/rejected/skipped counts, the reconciliation table with deltas and
   explanations, idempotency ids before and after, file hash results, privilege test results;
7. `cargo test --workspace` exact counts;
8. live systems changed: yes/no with scope (MJCCv1 must read "no writes"; staging schema names
   created or dropped);
9. blockers, who unblocks them, and daily slip;
10. rollback: what a revert looks like;
11. next bounded tasks.

State what is proven and by which command or link. Never say "done". No customer rows or secrets in
any committed file or in chat.

## 10. How this is reviewed

The auditor re-reads the diff, re-runs `rust` and every `migration-*` gate on your branch twice,
checks B1 stays green, and re-scores the matrix. A row is green only with a test that bites, a gate
run, and synthetic data. Defects are filed, not silently repaired. I will not approve production
cutover.
