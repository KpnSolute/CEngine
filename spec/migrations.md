# Schema migrations (`ce-serve migrate`)

Forward-only, versioned, ordered, checksum-pinned migrations for an installed schema, as required by `SCHEMA_FREEZE.md` section 9. Proven by `crates/ce-server/tests/migration_install.rs` and `migration_forward.rs`.

## Where things live

- Migration files: `crates/ce-store-pg/migrations/NNNN_name.sql`, embedded in the binary. `__SCHEMA__` stands for the installation's schema name.
- History: table `_ce_migration` in the installation schema. One row per attempt: `version`, `name`, `sha256` of the SQL text, `applied_at`, `applied_by`, `manifest_hash`, `duration_ms`, `status` (`applied` or `failed`), `error`. A partial unique index allows one `applied` row per version; failed rows are diagnosis only.
- `ce-serve install` creates the schema, then records and runs every embedded migration, so a fresh install is already current.

## Commands

```
ce-serve migrate --plan     # print the current version and what is pending; writes nothing
ce-serve migrate            # apply what is pending, one transaction per version
ce-serve migrate --adopt    # once, for a schema installed before this history existed
```

`CE_MIGRATE_ACTOR` sets the `applied_by` value (default `ce-serve`). The other environment variables are the same as for `install`.

## Rules, each with a test

| Rule | Behaviour | Test |
|---|---|---|
| Order | pending versions apply in ascending order; the binary's own list must be 1, 2, 3 without gaps | `migrate_applies_pending_versions_in_order_and_records_them`, `a_binary_list_that_is_not_in_order_is_rejected` |
| No-op | nothing pending changes nothing | `a_current_schema_is_a_no_op`, `a_fresh_install_records_every_migration_and_migrate_is_a_no_op` |
| Checksum | an applied version whose recorded sha256 differs from the binary's stops the run before any write | `a_changed_checksum_stops_everything` |
| Gap | applied versions must be exactly 1..k | `a_gap_in_the_history_fails_closed` |
| Ahead of binary | a database with a version the binary does not know fails closed | `a_database_ahead_of_the_binary_fails_closed` |
| Destructive | pending SQL containing the words `drop` or `truncate` is refused before any write (comments ignored) | `destructive_sql_is_refused_before_anything_is_written`, `only_real_drop_and_truncate_statements_count_as_destructive` |
| Plan | `--plan` reads only | `plan_reports_pending_and_writes_nothing` |
| Failure | a failing version rolls back and leaves a `failed` row with the database message; a corrected version then applies | `a_failing_migration_rolls_back_and_leaves_a_diagnosis` |
| No history | a schema without `_ce_migration` is refused until `--adopt` is run on purpose | `a_schema_without_history_is_refused_until_adopted` |

## Shipped migrations

1. `baseline`: nothing to run; records the schema as `install` created it before this history.
2. `migration_provenance`: `migration_runs`, `migration_id_map`, `migration_reconciliation_results`, `migration_errors`, `migration_artifacts` (SOP section 5), safe metadata only.

## Not covered

`migrate` does not change the entity tables generated from a manifest; a changed manifest is a separate, later step. There is no down migration by design.
