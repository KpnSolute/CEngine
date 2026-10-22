# Run the real-data rehearsal

**Audience:** the operator (principal). The auditor does not run the rehearsal; this is the gate
that says when it may start and exactly how to run it when it does.

**Current status: BLOCKED.** Loading and reconciliation are not implemented, so the prerequisites
below cannot be met yet. Do not run against real data before the gate passes.

---

## What "unblocked" means — all of these, not some

The rehearsal starts only when every line is true. Each is checkable; each names where the evidence
lives. A tick with no evidence is not a tick.

| # | Prerequisite | Evidence that satisfies it | Where it is recorded |
|---|---|---|---|
| 1 | Loading works on the fixture set | `migration-fixture-test` green on the worker's branch, with a run link | PR checks |
| 2 | Reconciliation passes on fixtures with zero unexplained deltas | `reconciliation-test` green; every `migration_reconciliation_results` row has `delta = 0` or an explanation committed in `docs/MIGRATION_MAP.md` | PR checks + matrix row R1–R6 |
| 3 | Re-run is idempotent | `migration-idempotency-test` green: second run yields the same target ids and zero new rows | PR checks |
| 4 | Rejected rows are reported, never silent | `migration_errors` has codes for every refused row in the fixture run | matrix row R7 |
| 5 | Privileges are proven | legacy reader is SELECT-only and the rehearsal writer cannot touch the live schema (`schema-security-test`) | matrix row S1–S3 |
| 6 | The authorisation is current | `spec/mjcc-backend-complete-by-wednesday.md` §2c present and not expired | that file |
| 7 | Credentials exist | a read-only legacy connection and an isolated rehearsal target, no other privileges | owner's secret store |
| 8 | The owner has stated "start" | an entry in this file, dated and signed by the owner | this file |

If any line is false, the answer is: not yet.

## How to run it

1. **Create the snapshot.** Take a read-only export of MJCCv1. Record:
   - snapshot time (UTC);
   - the export checksum (sha256 of the dump);
   - the read-only role used, and that it has no write grants.
   Write all three into the run record below.
2. **Create the rehearsal target.** A fresh schema on staging, named `ce_mjcc_rehearsal_<yyyymmdd>`.
   Nothing else is in it. Confirm `ce_mjcc_prod` does not exist anywhere reachable from this session,
   and that no connection string used points at MJCCv1 for writes.
3. **Run the migration** into the rehearsal schema. The run writes one `migration_runs` row
   (`planned` → `running` → `reconciling` → `passed` or `failed`) with the snapshot hash and time.
4. **Run reconciliation.** Every check from SOP §18 is executed and stored. The run only moves to
   `passed` if every delta is zero or explained.
5. **Record the result** in the table below, counts and totals only. No customer rows, no names, no
   payloads, no file contents.
6. **Expire the credentials.** Revoke the read-only role and drop the rehearsal schema (or keep it
   only if the owner wants a second look, and say so). Mark the run `cancelled` if it was stopped
   early.

## Run record

| Field | Value |
|---|---|
| Status | **BLOCKED — loading and reconciliation not implemented** |
| Owner "start" statement | *(empty)* |
| Snapshot time (UTC) | — |
| Snapshot sha256 | — |
| Read-only role | — |
| Rehearsal schema | — |
| `migration_runs.id` | — |
| Rows migrated / rejected | — |
| Reconciliation result | — |
| Credentials expired at | — |

## What the auditor does after it runs

- Re-reads the `migration_runs` and `migration_reconciliation_results` rows; does not take the
  summary on trust.
- Checks the snapshot hash matches the dump and the dump was taken before any run.
- Checks no live schema was written: the legacy role's grants, and the audit table for the live
  project (if it records writes at all) or the role's `pg_stat` write counters.
- Then either marks H1 green with the evidence or files the defects.

The auditor does not run the rehearsal and does not decide that it passed. The auditor verifies it.
