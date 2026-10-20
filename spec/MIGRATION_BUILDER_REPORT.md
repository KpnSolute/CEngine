# Migration builder report, 2026-10-20

Nothing here is called done. Each line says what is proven and by which command.

## 1. Branches and commits

- CEngine, `claude/project-thread-v5aqkl` (PR #92): migrate command and provenance (5aef206, 22741da), delivery-test fix (ce521ab), roles and isolation tests plus M10 (358a883), provenance writers and three gate tests (latest on the branch).
- MJCC-workspace, `claude/migration-map-and-extractor` (PR #12): map and extractor (5f5f15d), Codex fixes (4c622e6), owner-approved rows and `--plan-only` (2f8b4df).
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

- M10: `migration_install.rs::a_schema_installed_at_an_older_version_migrates_forward_to_current` (history shows install row then v2 by `tester`, checksum equal, `plan.current == 2`). Passes.
- P6: not built. The audit path has no way to import an event with its original actor and timestamp. That needs an engine decision first.

## 6. G5 to G8: what exists

Built and tested (synthetic data, local Postgres 16):

- Provenance writers (`ce_store_pg::provenance`): run lifecycle, id map, rejection, reconciliation check, run finish.
- Idempotency (`migration_idempotency.rs`, 2 tests): 50 mapped rows, second run creates 0 and returns the same 50 target ids; a source id cannot change entity.
- Reconciliation (`migration_reconciliation.rs`, 4 tests): one cent off fails; a delta passes only with a committed explanation; a run passes only with at least one check, none failed and no rejection; a rejected row keeps a code and a sha256, and the payload text is not stored.
- Privileges (`migration_privileges.rs`, 5 tests): SELECT-only reader, writer limited to its schema, runtime role without DDL or history writes, a broader reader detected. Mutation check: removing the `_ce_migration` revoke makes the runtime test fail.
- Tenant isolation (`migration_tenant_isolation.rs`, 3 tests): rows, files (including a forged file pointer) and provenance per tenant; the same legacy id maps to different targets per tenant.
- Performance (`migration_performance.rs`): 10,000 mappings, first run 9.8 s, rerun 6.6 s, ceiling 60 s, on this sandbox.

Not built, so the matching rows stay red:

- The loader (G5): nothing reads MJCCv1 rows and writes CEngine entities. Parents-then-children, history reconstruction and the SOP section 18 reconciliation checks against real totals do not exist. The reconciliation tests prove the check rules, not the migration.
- Audit actor/timestamp preservation (G6) and file copy verification (G7). The `main` blob backend is in memory; the S3 store would need to come first for real file work.

## 7. Test counts

`cargo test --workspace -q` with `CE_REQUIRE_DB=1`: 437 passed, 0 failed. `cargo clippy --all-targets -D warnings` on `ce-server` and `ce-store-pg`: clean. Extractor: 16 tests OK.

## 8. Gates

CI on the latest head is recorded in PR #92 once it finishes; at 358a883, `fresh-install-test`, `schema-security-test` and `tenant-isolation-test` were green, `migration-static-check` and `migration-fixture-test` red for the cross-repo reason in section 4. I have not edited any gate.

## 9. Rehearsal and live systems

Rehearsal: blocked, as instructed (loader not built, no owner start). Live systems changed: no. MJCCv1: no connection and no writes. Schemas created and dropped: only throwaway ones in a local Postgres. Nothing was run against the testing project this round.

## 10. Blockers

- PR #12 must merge (Othniel or the owner) for the static and fixture gates to see the extractor. Slips: static check and fixture gate stay red until then.
- Loader design: which API or SQL path writes CEngine entities with a source-supplied id and original timestamps, and how P6 audit import works. Owner/auditor decision; slips the Wednesday history rehearsal.

## 11. Rollback

Revert the commits listed in section 1 on each branch. The two new migrations are additive; nothing is dropped. Roles exist only as generated SQL; none were applied to a shared database.

## 12. Next bounded tasks

1. Decide the loader write path and P6 import (needs a decision).
2. Loader skeleton and parents (`location`, `inventory_category`, `vendor`, `staff_member`) against the fixtures.
3. Stock items, then weekly history and reconciliation to the cent on `normal_month`.
4. Refresh `docs/CONTRACT_PARITY.md`.
