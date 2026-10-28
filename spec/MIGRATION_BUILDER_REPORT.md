# Migration builder report, 2026-10-22

Nothing here is called done. Each line says what is proven and by which command.

## 1. Branches and commits

- CEngine, `claude/project-thread-v5aqkl` (PR #92): migrate command and provenance (5aef206, 22741da), delivery-test fix (ce521ab), roles and isolation tests plus M10 (358a883), provenance writers and three gate tests (latest on the branch).
- MJCC-workspace PR #12 (map and extractor): merged by the owner on 2026-10-22 (52aaf06). PR #13, `claude/migration-loader-plan`: loader plan and synthetic fixture tables (latest 09b8399).
- MJCC-workspace, `claude/legacy-formulas-reorders-audits` (PR #11): over-pull audit report.

## 2. G1: the flaky delivery test

- Reproduced: `cargo test -p ce-server --test plugin_deliveries jobs_waiting` failed 1 of 5 runs (left 0, right 1 at line 408).
- Ordering hypothesis refuted. In the failing run the relay had already answered `delivered: 1` (the earlier assertion passed). The failing line counted `done` jobs from `list("archiver__job")`, which read one unsorted page of 100 out of 121 rows and could miss the done one. The relay is unchanged.
- Fix: the test helper now follows the page cursor. The abort (SIGABRT) came from the store dropping inside the runtime when an assertion panicked; the router is now dropped on a thread of its own.
- Proof: 10 of 10 clean local runs of the whole file. With a deliberately wrong assertion the test now fails with a normal message and the other two tests still run (no abort). CI link: see section 8.

## 3. G3: map rows

Before: the eight rows were `omitted` with "approved omission needed" (two rows) or no approval note, and `flow_assignments` targeted `cafeteria_event`. After (PR #12, 2f8b4df): `github_sync_queue`, `error_logs`, `staff_login_throttle`, `sso_handoffs`, `lunchvoice_sso_handoffs`, `api_keys`, `ai_provider_keys` are `omitted` with `owner-approved: 2026-10-20` and a reason; `flow_assignments` is `archive-only` with no target. `snack_bar_entity_rates` is unchanged. `python -m unittest discover -s tools/tests`: 16 tests OK, including one that asserts these dispositions.

## 4. G12

Fixture command and the control-plane history answer are in `spec/migrations.md`. The command is `python tools/migrate_v1.py --fixtures tests/migration/fixtures --map docs/MIGRATION_MAP.md --plan-only --target <label>`. The gate currently checks out MJCC-workspace `main`, which lacks the extractor until PR #12 merges, so `migration-static-check` rules 4 and 5 and `migration-fixture-test` fail for that reason only.

## 5. G13

- M10: `migration_install.rs::a_schema_installed_at_an_older_version_migrates_forward_to_current`. Passes.
- P6: `migration_audit.rs` (4 tests). A legacy event is imported with its original actor and timestamp and the `legacy-import` request id; a re-import adds no second event.

## 6. G5 to G8: what exists

Built and tested (synthetic data only, local Postgres 16, no MJCCv1 connection):

- Provenance writers, idempotency, privileges, tenant isolation and performance: unchanged from the earlier round (`migration_idempotency.rs`, `migration_privileges.rs`, `migration_tenant_isolation.rs`, `migration_performance.rs`).
- Loader (`ce_store_pg::loader`, `import`): plan-driven, parents then children, one transaction per row, legacy id mapped, original timestamp and actor kept, legacy state kept, unmapped source columns refuse the whole load, a missing parent or a refused row is rejected by table, id and code with a payload hash only. Transforms: format, month start/end, lookup, ordinal, value map; derived match columns.
- Engine recompute: `settle_imported` recomputes derived fields after a load; they are never copied.
- Reconciliation (`migration_reconciliation.rs`, 12 tests): inventory totals (received, pulled, ending quantity, ending value) equal the legacy formulas to the cent using the `mjcc-legacy-valuation` plugin; a cent off fails; per-invoice goods total and delta equal the legacy figures and only the invoice that adds up is `reconciled`; line totals equal; price history keeps status, decision, prices and the month-0 offset; an undeclared status is refused by name; a second load creates nothing.
- Week locks (update mode, audited under `legacy-import`; a fifth week is refused by name) and staff (role mapped, PIN never copied; a scan of every target table finds no PIN) in `migration_reconciliation.rs`, now 14 tests.
- Stock lines carry the item's par level (plan derives `par_level` from `inventory_items`; reverting the plan makes `each_stock_line_carries_the_par_level_its_item_had` fail).
- Menus (items, cycle days keyed by `cycle_day`, slots, suggestions with their state), budget lines and monthly actuals (0-based legacy month shifted to the period), snack bar (products, rates, transactions, items, daily sales). Snack bar sale totals are recomputed by the engine and checked against the legacy figures per sale and per column; a one-cent change in the fixture fails the test. `migration_reconciliation.rs` is now 19 tests. The fixtures validate against the workspace map (23 tables).
- Fields the reports group on (plugin 0.5.0 contract, `LOADER_FIELDS_0_5_0.md`): invoices carry the period of their date and the legacy three-week bucket (`week_of_day`: `min(3, (day-1)//7+1)`), snack transactions carry the period of their date, stock items carry their category (stock lines compute theirs), and one stock_item_detail row per item with `active`. Generic loader pieces: a `part` (year/month/day) on date match values and a `bucket` transform; the MJCC rule lives in the plan. The test plugin copy in `crates/ce-plugin/tests/fixtures/mjcc-legacy-valuation` is synthetic and mirrors the [[extend]] fields. `migration_reconciliation.rs` is now 22 tests.
- Rehearsal driver: `ce-serve migrate-load --plan P --data DIR --checks C` plans a run, loads `DIR/<table>.json`, lets the engine recompute derived fields, runs the checks file (`tests/migration/checks/mjcc_checks_v1.json`: legacy sum/count against a read-only SELECT on the target, no tolerance) and closes the run `passed` or `failed`. It refuses a schema whose name contains `prod` and never reads the legacy database. Proven on synthetic data in two ways: two tests in `migration_reconciliation.rs` (a one-cent difference and a rejected row both fail the run; a second drive writes nothing) and a smoke run of the built binary against the real MJCC template on local Postgres 16 (12 checks pass).
- Not built: `week_gross`, `events`, `daily_operations_logs`, `opening_checklist_items` (no DDL in the Compute repo; they need a read-only MJCCv1 column list, which needs the owner's explicit go).
- Cost budgets (owner choice 2026-10-25: add weekly fields): six weekly planned pull/renewable fields added to the template's `cost_budget`; allotment, notes and every weekly plan reconcile to the cent, 0-based legacy month shifted to match the period; test `budgets_keep_the_allotment_and_every_weekly_plan_to_the_cent`, file now 15 tests. Synthetic data only.
- Files (`migration_files.rs`, 6 tests, G7): source hash and size checked, copy read back and hashed, link only after that, missing/empty/mismatch/missing-record/corrupt copy rejected by code with no leftover bytes, duplicates counted, orphan sources listed, re-run changes nothing, `verify_files` finds missing, tampered, cross-tenant and wrongly owned links.

Not built, so the matching rows stay red:

- Real MJCCv1 column conversions beyond the synthetic slice: `monthly_inventory` weekly columns, `week_gross`, menus, HACCP, events, budgets.
- Rejected-row reconciliation across the whole run; weekly gross, budgets, menus, HACCP, events.
- `MIGRATION_MAP.md` says `week_status` goes to `period_week`; the template keeps the four lock flags on `period`, so the loader sets them there. The map row needs the owner's wording.
- Week locks keep who locked and when only as the audit event's request id and the load time; the legacy `locked_by` and `locked_at` are ignored by the plan.
- Files against a real blob backend: the `main` backend is in memory, so G7 is proven with the trait and `MemoryBlobs` only.
- The Python extractor still does not load; only the Rust loader does.

## 7. Test counts

`cargo test --workspace -q` with `CE_REQUIRE_DB=1`: 464 passed, 0 failed. `cargo clippy --workspace --all-targets -D warnings`: clean. Extractor: 16 tests OK; fixtures validate against the map (9 tables).

## 8. Gates

On head e940156: `fresh-install-test`, `schema-security-test` and `tenant-isolation-test` green. `migration-static-check` fails rules 4 and 5 because the job checks out only CEngine (auditor-owned layout fix). `migration-fixture-test` now passes its checkout with `MJCC_WORKSPACE_TOKEN`, then fails because MJCC-workspace's default branch is not `main` (owner setting). `migration-audit-report`: 15 green, 1 partial, 27 red at e940156. I have not edited any gate.

## 9. Rehearsal and live systems

Rehearsal: blocked; `MIGRATION_REHEARSAL_GATE.md` is not satisfied and there is no owner "start". Live systems changed: no. MJCCv1: no connection and no writes. Only throwaway schemas in a local Postgres.

## 10. Blockers

- MJCC-workspace default branch must be `main` (owner setting) for the fixture gate.
- Static-check rules 4 and 5 need the two-repo layout (auditor).
- Real column conversions need the real MJCCv1 schema; reading it is a read-only step that needs the owner's say.

## 11. Rollback

Revert the commits on each branch. Migrations are additive; nothing is dropped. Roles exist only as generated SQL.

## 12. Next bounded tasks

1. Weekly gross, budgets and HACCP slices with fixtures.
2. Owner wording for the `week_status` map row.
3. Whole-run reconciliation of rejected rows.
4. Refresh `docs/CONTRACT_PARITY.md`.
