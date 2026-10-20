# Final worker handoff: MJCC migration program (auditor-authored v3)

**Issued by:** migration auditor, 2026-10-20
**Supersedes:** `MIGRATION_BUILDER_PROMPT_V2.md` and `CLAUDE_MIGRATION_IMPLEMENTATION_PROMPT.md`
**Builder:** Claude Code
**Owner:** the principal. This prompt does not carry production or cutover authority.

Everything below is based on the tree as it stands, the scored matrix, CI runs read directly, and
the owner's decisions of 2026-10-20. Where this prompt and an older document disagree, this prompt
wins for the work; the canonical documents win for the contract.

---

## 0. Evidence you are starting from — verify, do not trust

| Fact | Where to check |
|---|---|
| Matrix: 43 rows, 15 green, 1 partial, 27 red | `docs/MIGRATION_AUDIT_MATRIX.md` (re-score section) |
| PR #92 head `22741da`: migration engine, 13 tests, provenance tables; M1–M9, M11, P1–P5 green | `gh pr view 92 -R KpnSolute/CEngine` |
| PR #12 head `4c622e6`: map for 81 tables, extractor, 14 unit tests | `gh pr view 12 -R KpnWorld/MJCC-workspace` |
| `main` CI has been red twice on the same test | `spec/MIGRATION_BLOCKERS.md` B1, run `39369255125` |
| Loading is not implemented; the extractor says so | `MJCC-workspace/tools/migrate_v1.py --help` |
| Rehearsal is blocked | `spec/MIGRATION_BLOCKERS.md` B2 |

Read, in this order, before writing code:
1. `spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md`
2. `spec/SCHEMA_FREEZE.md`
3. `spec/mjcc-backend-complete-by-wednesday.md`
4. `spec/MIGRATION_BLOCKERS.md` — **your first two tasks are there**
5. `docs/MIGRATION_AUDIT_MATRIX.md`
6. the two open PR diffs and the code they touch

## 1. Decisions that are closed

Do not reopen these or spend time proposing alternatives:

- CEngine is the shared runtime; MJCC is one tenant template on it.
- One CEngine control plane; one data plane per production customer. MJCC's data plane is MJCCv1
  (`mgvyylvmkxhhataavqjz`); its production schema is `ce_mjcc_prod`.
- `ce_mjcc_staging` (`aqkhebljkagytsqtkkly`) is throwaway and is the temporary control plane this
  week, with control rows marked temporary. Full write access there. **Never** write MJCCv1.
- No customer payloads, files or secrets in the control plane. Keys stay in the tenant Vault.
- Rehearsal: read-only snapshot, isolated target, no dual writes, blocked until §B2 clears.
- Compute becomes read-only for 14 days after a signed cutover.
- The UI lives in MJCC-workspace. The AI provider adapter is temporary; the template contract is
  capability-based.
- `QUESTIONS_TOPOLOGY.md` (in `KpnSolute/Compute/docs/mjcc-migration/`) is a **checklist only**:
  mark entries these documents settle as "resolved by `<doc>`" and leave the rest open. Never wait
  on it.
- Migration map, owner-approved 2026-10-20 — apply exactly (see §3).

## 2. Work package B1 — clear the blocker before anything else

`crates/ce-server/tests/plugin_deliveries.rs:408` `jobs_waiting_for_a_retry_never_hide_a_due_one`
fails on `main` and again on re-run. Full evidence in `spec/MIGRATION_BLOCKERS.md` B1.

Do this:
1. **Reproduce locally** with the CI database settings and `cargo test -p ce-server --test plugin_deliveries jobs_waiting_for_a_retry_never_hide_a_due_one -- --nocapture`. Report pass/fail counts over at least 5 runs; a test that passes 4 times in 5 is still broken.
2. **Confirm or refute the ordering hypothesis**: the relay lists `status in (queued, in_flight)` with `sort: []` (`crates/ce-server/src/deliveries.rs` around the `ListRequest`), so paging follows whatever order Postgres returns while the row set is changing under it. Log page sizes and the page the due job lands on. If the due job is consistently delivered when a deterministic sort (`created_at, id`) is added, say so with the run that shows it.
3. **Fix the cause, not the assertion.** If it is ordering, give the relay a deterministic order. Do not raise `MAX_PAGES` or shrink the 120 waiting jobs to make it pass, and do not add sleeps.
4. **Fix the failure path**: when the assertion fails the test panics inside the Tokio runtime and
   then drops the rig from `spawn_blocking`, which panics again with
   `Cannot start a runtime from within a runtime` and aborts the binary. A failed assertion must
   leave a readable failure, not a SIGABRT. Restructure the cleanup so the failure path still
   reports properly.
5. **Prove it**: five consecutive clean runs locally, plus a CI run link on your branch showing
   the `rust` job green.

Do not touch this test's expectations unless you show the expectation is wrong. If you conclude it
is, state why with evidence and raise it as a question, not a change.

## 3. Work package B3 — apply the owner-approved map

In `MJCC-workspace/docs/MIGRATION_MAP.md`, on the eight rows listed in `spec/MIGRATION_BLOCKERS.md`
B3:
- seven rows → `omitted`, with the note "owner-approved 2026-10-20" and the one-line reason from
  B3, replacing the current "approved omission needed from the owner";
- `flow_assignments` → `archive-only`, target `task` (workspace.core) when it exists. **Remove**
  the `cafeteria_event` target; do not create calendar events from assignments.
- `snack_bar_entity_rates → snack_bar_rate` stays as it is.
- Update `tools/tests/test_migrate_v1.py` and the fixture rows if any test asserts the old
  disposition. The static gate already checks every audited table appears; keep it passing.

## 4. Work package B5 — state the fixture contract

The fixture gate must run without a database. The fixtures and the map live in MJCC-workspace; the
gate runs in CEngine; no cross-repo token exists. Choose one and write it down in
`spec/migrations.md`:
- (a) `migrate_v1.py --plan-only --fixtures DIR --map FILE` that touches no network or database; or
- (b) fixtures and map copied into CEngine `tests/migration/` by an explicit export step you
  script, with the MJCC copy remaining the source of truth.
Give the auditor the exact command line you want the gate to run. The auditor writes the gate to
your contract; you do not edit the gate.

Also answer, in the same doc: where the **control plane's** own migration history lives when the
control plane is migrated (Freeze §9). A one-paragraph decision is enough; it does not have to be
implemented this week.

## 5. Work package to close M10

Add one test that installs an **older** supported version and migrates forward:
`TypedPgStore::install_with_migrations(m, schema, cfg, &older_list)` → `migrate(&embedded())` →
assert `migration_history` shows the forward step with the right checksums and a `plan` showing
`current == 2`. This is the only migration-engine row still `partial`.

## 6. Work package to close P6

Show migration provenance is separate from business audit: a reconciliation test that runs a
migrated record and asserts its `audit_event` keeps the **original actor and timestamp**, while the
migration itself is recorded only in `migration_runs` / `migration_artifacts`, never as the actor of
a business act. Ledger export logs are excluded from business audit.

## 7. Work package to start loading (E and R rows)

This is the bulk of the remaining red and it will not finish in one pass. Implement in this order,
each with its own test:

1. `--target` loader skeleton: open the target, write a `migration_runs` row (`planned` →
   `running`), refuse production and same-database targets (already done in the extractor —
   reuse it, do not duplicate).
2. Parents first: `location`, `inventory_category`, `vendor`, `staff_member`, `period`/`period_week`,
   `stock_item` + `stock_item_detail`. Record every write in `migration_id_map` in the same
   transaction as the entity write.
3. Children through the map: `stock_line` per period, `stock_movement` per week, `supplier_invoice`
   + `supplier_invoice_line`, `price_observation`, `menu_*`, `haccp_log`, logs and checklists,
   `budget_*`, `certification`.
4. Reconciliation pass per the SOP §18 list; write one
   `migration_reconciliation_results` row per check, with `legacy_value`, `target_value`, `delta`,
   and `pass` only when the delta is zero or explained in a committed note.
5. Rejected rows: every refusal writes a `migration_errors` row with the source table, source id,
   a code and a safe message — never the payload.
6. Idempotency: a second run over the same fixture set yields the same target ids and zero new
   records; test it.
7. Files: hash-compare copy with the five failure detections (missing, mismatch, duplicate,
   orphan, cross-tenant) — only after the database path works.

Do not start rehearsal. Do not touch MJCCv1. All fixtures synthetic.

## 8. Report format

Post in chat and save as `spec/MIGRATION_BUILDER_REPORT.md`:

1. branches and commits per repo;
2. B1: five local runs with pass/fail, the ordering evidence, the fix, and the CI run link;
3. B3: the eight map rows before and after;
4. B5: the chosen fixture contract and the exact command the gate should run; the control-plane
   history answer;
5. M10 and P6 tests, with names and output;
6. loading: per table, written / rejected / skipped counts, and reconciliation table with deltas
   and explanations;
7. idempotency: ids before and after the second run;
8. `cargo test --workspace` with exact counts;
9. live systems changed: yes/no with scope (MJCCv1 must be "no writes"; staging schema names
   created/dropped);
10. blockers, who can unblock, and daily slip;
11. rollback: what a revert looks like;
12. next bounded tasks.

State what is proven by which command or link. Never say "done". No customer rows or secrets in any
committed file or in chat.

## 9. What the auditor will do when you return

- Re-read the diff, not the summary.
- Re-run the `rust` and `migration-*` gates on your branch, twice, and check B1 stays green.
- Re-score every row; a row is green only with a test that bites, a gate run, and synthetic data.
- File defects rather than fixing them. If a gate is wrong against the SOP, I fix the gate and say
  so, as I did when removing my own rule 6.
- I will not approve production cutover. That stays a signed owner decision.
