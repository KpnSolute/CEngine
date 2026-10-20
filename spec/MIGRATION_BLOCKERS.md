# Migration blockers — worker must clear these

Auditor's register. Each entry names the exact failing thing, the evidence, whether it reproduces,
and what the worker must do. Nothing here is fixed by the auditor; the auditor only records.

---

## B1. `plugin_deliveries::jobs_waiting_for_a_retry_never_hide_a_due_one` fails on `main`

**Exact failing test:** `crates/ce-server/tests/plugin_deliveries.rs:408`,
`assert_eq!(done, 1)` — left `0`, right `1`.

**CI evidence:**
- Run `39369255125`, job `rust`, commit `74eb88d` (`main`), `cargo test --workspace`, step failed.
- Re-run of the same commit (`gh run rerun --failed`): failed again, **same test, same assertion,
  same left/right**, at `09:46:25Z`.
- The preceding test binaries all reported `ok`; the failure is isolated to `plugin_deliveries`.
- Two separate executions, identical result: **reproducible on CI**.

```text
thread 'jobs_waiting_for_a_retry_never_hide_a_due_one' panicked at crates/ce-server/tests/plugin_deliveries.rs:408:5:
assertion `left == right` failed
  left: 0
 right: 1
```

**Reproduced on `main` at `74eb88d`.** That commit touches only `.github/`, `docs/` and
`scripts/ci/`, so this is not caused by audit changes; `plugin_deliveries.rs` last changed in
`d81d9c7` ("deliveries: page past waiting jobs…").

**Suspected nature: not a timing flake — a pagination defect.**
- The test creates **120** waiting jobs (`not_before: 900_000`) through the API, one request each,
  then expects the one due job to be delivered.
- `crates/ce-server/src/deliveries.rs:22-24`: `BATCH = 50`, `MAX_PAGES = 20` → one run reads at
  most **1 000** rows, so the 120 waiting jobs fit inside the page budget; the "ran out of pages"
  theory does not explain it.
- The stronger suspect is **ordering**: the relay lists `status in (queued, in_flight)` with
  `sort: []`, so Postgres is free to return rows in physical order. The due job is created
  *before* the 120 waiters, so under a different plan/heap order it can sit beyond the first page
  of empty `sort` ordering, and `after`-cursor paging walks whatever order the planner chose.
  `store.list` also skips blank cursor fields from `sort: []`, which means paging over an unordered
  query is only safe while the row set does not change — it does change as the relay claims a job.
- To confirm: instrument the run to log `page.len()` per page and the id of the due job vs the page
  it lands on. The worker owns that investigation.

**Second, separate defect in the same run:** when the assertion fails, the test panics *inside* the
Tokio runtime and then `spawn_blocking(move || drop(owner))` → `r.finish()` panics with
`Cannot start a runtime from within a runtime`, aborting the process (`SIGABRT`). So a single
assertion failure turns into a crashed test binary that hides the remaining tests' results. The
test must not drop the rig from inside the runtime on the failure path.

**Assigned to:** worker. See `spec/MIGRATION_BUILDER_PROMPT_V3.md`, work package B1. Owner of the
fix is the worker; the auditor re-checks by re-running the `rust` job twice.

---

## B2. Real-data rehearsal is blocked

**Status:** BLOCKED. Not a go.

**Why:** the SOP prerequisites are unmet: loading is not implemented
(`tools/migrate_v1.py` says so itself), so no reconciliation has run, no `migration_runs` row
exists, and the gate set (E, R, A, F, S rows in `docs/MIGRATION_AUDIT_MATRIX.md`) is red. Starting
a real snapshot now would produce a rehearsal with nothing to verify it against.

**Unblocks when:** loading and reconciliation pass on synthetic fixtures (`migration-fixture-test`
and `reconciliation-test` green), the §2c credentials exist, and the owner states the start.

**Nothing to do this week:** do not run rehearsal early.

---

## B3. Owner-approved migration-map decisions, not yet in the map

Recorded 2026-10-20 by the owner as **owner-approved 2026-10-20**. The auditor does not edit the
map; the worker applies these exact rows in `MJCC-workspace/docs/MIGRATION_MAP.md`:

| table | disposition | note to write |
|---|---|---|
| `github_sync_queue` | omitted | owner-approved 2026-10-20 (runtime queue, rebuilt) |
| `error_logs` | omitted | owner-approved 2026-10-20 (operational noise, not business history) |
| `staff_login_throttle` | omitted | owner-approved 2026-10-20 (runtime state; rebuilt empty) |
| `sso_handoffs` | omitted | owner-approved 2026-10-20 (expired handoffs, runtime state) |
| `lunchvoice_sso_handoffs` | omitted | owner-approved 2026-10-20 (expired handoffs, runtime state) |
| `api_keys` | omitted | owner-approved 2026-10-20 (secrets never copied; re-issued) |
| `ai_provider_keys` | omitted | owner-approved 2026-10-20 (secrets stay in tenant Vault) |
| `flow_assignments` | archive-only | owner-approved 2026-10-20; target `task` (workspace.core) when it exists; **must not** become `cafeteria_event` |

The worker replaces "approved omission needed from the owner" with the owner-approved note on those
seven rows, and changes `flow_assignments` from `cafeteria_event` to `archive-only`.

---

## B4. Audit-gate corrections made by the auditor in this pass

For the record, and so the worker does not build against a wrong gate:
- The auditor's earlier "rule 6" (general CI must set a migration `--experimental` flag) was based
  on a flag that does not exist in the tree. It has been **removed** from
  `scripts/ci/migration_static_check.sh`.
- The earlier fixture job checked out `KpnWorld/MJCC-workspace` through a token that does not
  exist. The gate is **restored** to the version at `0be11f1`; the fixture job will be re-specified
  in the worker prompt (B5) once the worker's layout is settled, rather than guessed.
- No application or runtime code was touched by these corrections.

---

## B5. Open design questions for the worker (not blockers)

1. Where does `migration_*` provenance live for the **control plane** when the control plane
   itself is migrated? (Freeze §9 says the control plane keeps its own history; no table yet.)
2. The fixture job needs to run against a tree with both repos. Decide and state whether
   `migrate_v1.py` gets a `--plan-only` that needs no database (so the fixture gate is hermetic).
   The auditor will then write the gate to that contract.
