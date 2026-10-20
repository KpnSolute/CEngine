# DRAFT TEMPLATE — AUDITOR MUST REWRITE BEFORE HANDOFF

**Status:** Seed only. This file is not yet the final worker prompt. The migration auditor must first inspect the current repositories, build/update the migration audit matrix and CI gates, then replace this draft with an evidence-based prompt containing the actual remaining implementation gaps. Do not hand this draft directly to Claude Code.

# Claude Code Handoff: implement MJCC historical migration and CEngine migration safety

You are the primary builder for the MJCC historical migration program.

Do not stop for ordinary implementation questions. Use the repository SOPs as your operating companion and keep them synchronized with implementation.

## Authority order

Read and follow, in this order:

1. spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md
2. spec/mjcc-backend-complete-by-wednesday.md
3. spec/SCHEMA_FREEZE.md if present
4. accepted repository governance and Loom decisions
5. deploy/render/STAGING.md for current runtime limitations
6. existing code/tests as implementation evidence

If an implementation detail is ambiguous but can be resolved from these sources, choose the safest reversible option, document it, amend the relevant SOP/document when necessary, and continue.

Do not ask the owner ordinary architecture or coding questions already answered by these documents.

## QUESTIONS_TOPOLOGY rule

QUESTIONS_TOPOLOGY is not a blocker.

If the file exists locally, use it only as a checklist:
- mark already-decided questions resolved by reference to canonical docs;
- do not reopen resolved decisions;
- leave genuinely unresolved questions open;
- if it conflicts with newer canonical decisions, update the question status rather than implementing stale wording.

If it is missing, continue. Do not wait for it.

The following are already decided:
- separate CEngine control plane and tenant data planes;
- one dedicated production data environment per customer;
- MJCC production data plane is MJCCv1;
- MJCC target schema is ce_mjcc_prod;
- ce_mjcc_staging is throwaway/test-only;
- no customer business data in the CEngine control-plane database;
- real-data rehearsal uses a read-only source and isolated target;
- no dual writes;
- Compute becomes read-only at cutover for 14 days;
- MJCC UI lives in MJCC-workspace and CEngine is the business API;
- old MJCC data is transformed into new CEngine tables;
- archived files move to tenant-controlled storage;
- current AI provider adapter is temporary and the permanent template contract is capability-based.

## Mission

Implement the migration foundation required to move legacy MJCC history into the new CEngine tables safely and repeatably.

You own the builder side of:

- ce-serve migrate
- migration version/history contracts
- migration metadata/provenance
- legacy-to-target ID mapping
- MJCC source-to-target ETL
- reconciliation
- idempotent re-run behavior
- protected rehearsal tooling
- file/object migration verification
- delta migration
- migration docs and tests
- fixes required to satisfy migration CI

Generic engine/runtime work belongs in KpnSolute/CEngine.

MJCC-specific mappings, ETL, sanitized migration fixtures and reconciliation goldens belong in KpnWorld/MJCC-workspace.

KpnSolute/Compute is read-only source/reference. Do not move new implementation back into Compute.

## Preserve existing work

Before editing:
- inspect git status in both repos;
- preserve unrelated local changes;
- do not reset, clean, checkout over, delete or rewrite unrelated work;
- use a dedicated branch/worktree if necessary.

No real customer data or secrets may be committed.

## Work package 1: ce-serve migrate

Implement the reviewed forward migration path described in the schedule.

Required behavior:

- forward-only migrations;
- versioned migration records in each installation schema;
- record at least version, name, SQL/content hash, applied_at, applied_by or actor identifier, manifest hash where available, duration and status;
- additive/backward-compatible operations first;
- no automatic destructive migration this week;
- plan/dry-run mode;
- no-op when current;
- fail closed on changed checksum;
- fail closed on version gaps;
- fail closed when DB migration version is ahead of the binary;
- fail closed on prohibited destructive operations;
- transaction boundaries explicit;
- failure leaves a diagnosable state and does not claim success;
- fresh install followed by migrate must be tested.

Do not implement a vague "execute arbitrary SQL folder" feature without validating order and immutability.

Update deploy/render/STAGING.md only after the migration command actually exists and tests prove it.

## Work package 2: migration provenance model

Implement the contract from the migration SOP using established naming conventions.

Required concepts:

- migration run
- source snapshot identity/time/hash
- code/migration version
- source-to-target ID mapping
- reconciliation result
- migration error
- safe migration artifact metadata

Do not store raw unrestricted customer payloads in migration error/audit tables.

Stable mapping is mandatory. Re-running the same source record must resolve to the same target record.

## Work package 3: MJCC migration map

In MJCC-workspace create/update docs/MIGRATION_MAP.md.

Inventory every legacy source table relevant to the migration.

Every source table must have an explicit disposition:

- migrated
- transformed
- archive-only
- intentionally omitted with documented reason

At minimum implement mappings for:

- user/staff identity and historical attribution
- roles/location scopes
- locations
- categories
- vendors
- inventory items/SKUs/aliases
- periods and operational weeks
- opening state
- inventory transactions/movements
- receipts
- pulls/issues
- invoices
- invoice lines
- accepted prices and price-review history
- menu structures and meal periods
- HACCP/compliance
- daily logs/checklists/incidents/events
- certifications/training where target model supports them
- approvals/governed changes
- business audit history
- file/document metadata
- archive object references
- approved AI configuration/usage metadata without secret values

Do not copy provider API keys, service-role keys, passwords or PINs.

## Work package 4: ETL implementation

Build the migration tool in MJCC-workspace.

Use the naming/layout that best matches the repo, but keep extraction, transformation, loading and reconciliation separable enough to test.

Required properties:

- source read-only;
- target explicitly selected;
- target rehearsal schema cannot accidentally default to production;
- source project/schema and target project/schema printed in a safe startup summary;
- production target requires an explicit production mode and separate authorization later;
- mapping uses stable provenance keys;
- child records resolve target parent IDs through the mapping table;
- invalid/ambiguous rows are rejected explicitly, never silently coerced;
- rerun is idempotent;
- partial failure can be resumed or safely re-run;
- safe dry-run or plan mode where practical;
- run log records counts, rejected rows and reconciliation state;
- no customer rows written to git output.

Prefer using the CEngine API/validated write path where practical.

If a historical bulk-load path is necessary for performance or historical attribution, document why, isolate it, and run equivalent validation afterward. Do not bypass invariants just because migration is faster through SQL.

## Work package 5: historical inventory reconstruction

Do not migrate only current on-hand values.

Reconstruct history so the target preserves:

- opening quantity
- opening carried cost
- receipts by operational week
- pulls/issues by operational week
- accepted supplier prices
- price observations/reviews
- ending quantity
- ending value
- period state/locks
- source invoice linkage

CEngine must recompute historical results and reconcile them against legacy totals.

Use decimal math. Never use binary float equality for money.

## Work package 6: invoice and valuation reconciliation

Implement machine-verifiable reconciliation for:

- invoice count
- invoice-line count
- vendor mapping
- subtotal
- discount
- tax
- fuel/fees
- stated total
- calculated/net total
- accepted unit-price chronology
- price review status
- opening value
- received value
- ending value
- carried unit cost
- approved rounding/tolerance behavior

Unexplained deltas fail.

Do not silently rewrite old periods using current price rules.

## Work package 7: actor/audit preservation

Historical business events must remain historical.

Required behavior:

- preserve original actor when known;
- preserve original timestamp;
- preserve meaningful event/action identity;
- add migration provenance separately;
- do not attribute old business acts to the migration service;
- separate imported business audit from migration-system audit;
- do not bulk-copy HTTP request logs into the business audit unless explicitly mapped.

## Work package 8: staff/auth data

Migrate profile/identity information required for history.

Do not migrate reusable auth secrets.

Active users may require re-enrollment/first-login PIN reset according to the new auth contract.

Historical records must still resolve to historical actor identities even if that user is no longer active.

## Work package 9: file migration

Implement file/object migration for the approved MJCC archive destination.

For each object:

1. identify source object;
2. compute source SHA-256;
3. record size/MIME;
4. copy to target tenant bucket;
5. compute destination SHA-256;
6. compare;
7. create/update target document metadata;
8. link migrated business record;
9. mark verified only after hash/reference checks pass.

Must detect:
- missing source object;
- hash mismatch;
- duplicate content;
- orphan metadata;
- orphan required object;
- cross-tenant key/path.

The home archive must not remain a permanent runtime dependency after accepted migration.

## Work package 10: delta migration

The final cutover will need either a final full migration or a rehearsed baseline plus cutoff delta.

Implement support for reliable delta selection when source change timestamps/event identity allow it.

Requirements:

- same source ID resolves to same target ID;
- new records insert;
- approved changed records update deterministically;
- deletes/tombstones require explicit mapping, never guess;
- locked historical periods remain protected;
- delta passes the same reconciliation path as full migration.

## Work package 11: security boundaries

Implement/test separate roles in the migration test model:

legacy migration reader:
- SELECT only
- no source mutation

target migration writer:
- target schema only
- cannot mutate source
- cannot write another tenant

normal runtime role:
- separate from migration privilege

Tests must demonstrate the boundary, not merely document it.

## Work package 12: normal CI fixtures

Only synthetic or sanitized fixtures may enter git and normal CI.

Build realistic legacy-shaped fixtures that cover:

- normal inventory month
- multiple operational weeks
- over-pull history
- accepted and held price changes
- invoice totals
- role mappings
- closed/locked period
- file metadata and synthetic file object/hash
- one rejected/unmapped record
- repeated migration run
- two synthetic tenants

Do not use the real invoice archive as CI fixtures.

## Auditor CI gates you must make green

The migration auditor is expected to create or maintain these gates:

- migration-static-check
- fresh-install-test
- forward-migration-test
- migration-fixture-test
- reconciliation-test
- migration-idempotency-test
- tenant-isolation-test
- schema-security-test
- migration-performance-test
- migration-audit-report

Treat red CI as a contract failure.

Do not disable, skip or weaken a gate to get green.

If a gate is wrong relative to the SOP, document the conflict and fix the gate transparently.

## Protected Wednesday rehearsal

Real MJCC history may be used only under the written rehearsal authorization already recorded in spec/mjcc-backend-complete-by-wednesday.md.

The rehearsal must use:

- read-only source snapshot/export;
- separate writer limited to isolated rehearsal schema;
- source snapshot timestamp;
- source snapshot/export checksum where practical;
- migration run log;
- no production cutover;
- no legacy modification/deletion;
- no raw customer rows committed to git;
- safe counts/totals/hashes only in committed reports;
- credentials expired/removed at end of authorization window.

Do not reinterpret this as production authorization.

## Mandatory reconciliation before rehearsal can pass

Log and verify:

- row counts by mapped source/target
- legacy-to-target ID mapping integrity
- no orphan required relationships
- inventory opening/received/pulled/ending
- valuation
- invoice totals
- accepted price history
- menu/history counts
- active user/role mapping
- historical actor mapping
- business-audit preservation
- file object count/hash/reference
- period lock semantics
- rejected/unmapped report

Every difference must be explained. "Close enough" is not a status.

## One-writer cutover design

Do not implement dual writes.

The design remains:

before cutover:
- Compute writes
- CEngine validates/rehearses

at cutover:
- legacy writes stop
- final backup/snapshot
- final migration/delta
- reconcile
- explicit cutover decision

after cutover:
- CEngine writes
- Compute read-only for 14 days

Cutover itself requires separate authorization.

## Documentation updates

As implementation changes reality, keep documentation synchronized.

At minimum update/create as appropriate:

CEngine:
- spec/migrations.md
- deploy/render/STAGING.md
- spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md only when implementation discovers a real contract clarification
- relevant runtime docs

MJCC-workspace:
- docs/MIGRATION_MAP.md
- docs/migration/reconciliation documentation or equivalent
- daily/reliability notes required by the schedule

Do not let docs claim a migration feature exists before tests prove it.

## Required verification

Run all applicable existing tests plus new migration tests.

At minimum report:

- cargo test --workspace result
- fresh install result
- migrate --plan result
- install -> migrate result
- checksum mutation refusal
- version gap refusal
- destructive migration refusal
- sanitized ETL result
- idempotent second run result
- reconciliation result
- tenant isolation result
- privilege-boundary result
- file-hash result

If local tooling/environment prevents a test, create the smallest safe alternate verification environment and record exactly what was and was not executed.

## Final response format

Return a concise evidence report with:

1. branches/commits
2. files changed
3. migrations/schema contracts added
4. ce-serve migrate status
5. ETL status
6. mapping coverage
7. reconciliation coverage
8. CI gates and results
9. tests and exact counts/results
10. real-data rehearsal status
11. live systems changed: yes/no, with exact scope
12. remaining blockers
13. rollback/recovery state
14. next bounded tasks

Do not say "done" without executable evidence.

Start now by reading the canonical migration SOP and schedule, inventorying current migration/install code and the two repo worktrees, then implement the smallest dependency-correct sequence that drives the auditor CI from red to green.
