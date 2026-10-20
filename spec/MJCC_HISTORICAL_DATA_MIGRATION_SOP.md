# MJCC Historical Data Migration SOP

**Status:** Canonical migration contract
**Owner:** KpnSolute / CEngine
**Customer:** Miami Job Corps Center (MJCC)
**Source system:** legacy KpnSolute Compute / MJCC production
**Target runtime:** CEngine
**Production data plane:** MJCCv1 (mgvyylvmkxhhataavqjz)
**Production target schema:** ce_mjcc_prod
**Rehearsal target:** isolated, throwaway rehearsal schema only
**Related schedule:** spec/mjcc-backend-complete-by-wednesday.md

---

## 1. Purpose

This SOP defines how historical MJCC production data is securely transformed into the new CEngine data model.

The migration is not an archive copy and it is not a starting-balance import. Historical MJCC records become first-class records in the new CEngine tables, with preserved lineage, historical meaning, business totals, permissions, auditability, file references and rollback evidence.

> Historical MJCC data is transformed into the new CEngine schema as first-class production data. Legacy rows are not merely linked or archived. Migration must preserve lineage, historical meaning, accounting integrity, permissions, auditability and rollback capability.

A migration is not successful because SQL completed. It is successful only when the target CEngine system reproduces the required business meaning of the legacy data and every acceptance gate in this SOP passes.

---

## 2. Authority and topology

The production topology is already decided:

- CEngine has a separate control plane.
- Each production customer has a separate tenant data plane.
- MJCC's tenant data plane is MJCCv1.
- MJCC's production CEngine schema is ce_mjcc_prod.
- ce_mjcc_staging and rehearsal schemas are test-only and throwaway.
- Customer transactions, people, files, business audit entries, notifications, jobs and idempotency records remain in the tenant data plane.
- CEngine's control plane stores platform records and minimal aggregated usage, not customer business payloads.

No migration implementation may reinterpret staging topology as the production architecture.

---

## 3. Migration safety principles

1. Legacy production is read-only during extraction. The migration path must not require write access to the legacy MJCC schema.
2. One writer at cutover. Before cutover Compute is the writer. After cutover CEngine is the writer. There is no dual-write interval.
3. Rehearsal is isolated. Real-data rehearsal uses an authorized read-only snapshot/export and writes only into an isolated rehearsal schema.
4. Migration code is repeatable. The same migration program used for rehearsal must support the final cutover merge.
5. Idempotent import. Re-running a migration run or a cutover delta must not duplicate business records.
6. Lineage is permanent. Every migrated record must be traceable back to its legacy source identity through migration metadata.
7. Business rules win over raw row shape. Old rows are transformed into the final target model; the old schema is not copied forward merely because it exists.
8. No customer rows in git or normal CI. Real data never enters source control or normal GitHub-hosted CI artifacts.
9. No silent loss. Unmapped, rejected or ambiguous rows are reported explicitly and block acceptance unless formally dispositioned.
10. No silent historical rewriting. Historical actors, timestamps, prices, locks, approvals and events are preserved as historical facts, not re-attributed to the migration operator.
11. No secret migration shortcuts. Service-role credentials, provider keys and production write keys must not appear in commits, logs or CI artifacts.
12. Rollback is designed before cutover. Backup, restore, legacy read-only fallback and the 14-day rollback window are part of the migration contract.

---

## 4. Required migration roles

### 4.1 Legacy migration reader

Purpose: extract legacy MJCC records.

Required permissions:
- SELECT only on required legacy schemas/tables/views.
- read access to required storage metadata where necessary.
- no INSERT, UPDATE, DELETE, DDL or RPC that mutates production.
- no permission to alter auth users or credentials.

During rehearsal verification, the migration must fail closed if this role has broader write capability than expected.

### 4.2 Rehearsal/target migration writer

Purpose: create migrated data in the isolated rehearsal schema or final ce_mjcc_prod schema.

Required permissions:
- write only to the approved target schema and approved tenant storage destination.
- no mutation privileges on legacy production tables.
- no ability to modify another tenant data plane.

### 4.3 CEngine runtime role

The normal runtime role is separate from migration credentials. Migration must not depend on a permanently over-privileged runtime role.

---

## 5. Required migration metadata

The target installation must support durable migration provenance. Names may be adapted to established CEngine naming conventions, but the contract is mandatory.

### 5.1 migration_runs

Minimum fields:
- id
- installation_id
- tenant_id
- source_system
- source_project_ref
- source_snapshot_at
- source_snapshot_hash
- migration_version
- migration_code_commit
- started_at
- completed_at
- status: planned, running, reconciling, passed, failed, cancelled
- source_record_count
- target_record_count
- rejected_record_count
- initiated_by
- approved_by
- notes_safe

### 5.2 migration_id_map

Minimum fields:
- migration_run_id
- source_system
- source_table
- source_id
- target_entity
- target_id
- source_parent_id when needed
- created_at

Required uniqueness:
- one source identity maps to at most one target identity for the same migration lineage.
- re-runs resolve the existing target identity instead of creating a duplicate.

### 5.3 migration_reconciliation_results

Minimum fields:
- migration_run_id
- check_key
- scope_type
- scope_id
- legacy_value
- target_value
- delta
- tolerance
- status
- details_safe
- checked_at

### 5.4 migration_errors

Minimum fields:
- migration_run_id
- source_table
- source_id
- error_code
- safe_error_message
- payload_hash
- severity
- created_at

Do not store unrestricted customer payloads in error tables.

### 5.5 migration_artifacts

Tracks safe metadata for generated export/import artifacts:
- migration_run_id
- artifact_kind
- path_or_object_ref
- sha256
- size_bytes
- created_at
- expires_at
- contains_customer_data

Artifacts containing customer data must live only in approved tenant-controlled or protected migration storage and must never be attached to normal GitHub Actions artifacts.

---

## 6. Source-to-target migration classes

### 6.1 Must migrate into active CEngine tables

At minimum:
- staff/user identities needed for historical attribution
- active staff mappings and role/location scopes
- locations
- inventory items
- SKU/alias/catalog mappings
- categories
- vendors/suppliers
- periods and operational weeks
- opening inventory state
- inventory receipts
- inventory pulls/issues
- inventory counts where retained
- supplier invoices
- supplier invoice lines
- accepted supplier prices
- price observations/review decisions
- menu cycles
- menu days/slots/items
- meal periods
- HACCP records
- daily operations logs
- checklists and historical entries
- incidents
- events/calendar history
- certifications/training where included in the target model
- budget/cost history where required
- approvals and governed changes
- business audit history
- archived file/document metadata
- source references required for files
- AI usage/configuration metadata where explicitly mapped, excluding secret values

### 6.2 Transform and normalize

Examples:
- legacy status strings to target enums/states
- old role names to the target role ladder
- old category names/codes to canonical category identities
- legacy timestamps to timezone-aware UTC while preserving original meaning
- old foreign keys to target IDs through migration_id_map
- old file paths/object keys to the target document/blob model
- nullable/default behavior that changed between systems
- legacy source-control/staging rows into CEngine changeset/audit concepts
- old period lock states into CEngine lifecycle states
- old provider metadata into provider capability configuration without copying secret keys

### 6.3 Archive/reference only

Records may remain archive-only only when the migration map explicitly marks them non-operational and provides the reason. Typical examples:
- obsolete caches
- temporary processing rows
- superseded backup tables
- deprecated UI/session artifacts
- stale configuration no longer used by the product

Archive-only does not mean silently ignored. It must appear in docs/MIGRATION_MAP.md with disposition.

---

## 7. Mandatory migration map

MJCC-workspace/docs/MIGRATION_MAP.md must contain a row for every source table considered.

Required columns:

| Source | Target | Transformation | Key mapping | Reconciliation | Disposition | Notes |
|---|---|---|---|---|---|---|

Every legacy table in scope must be one of:
- migrated
- transformed
- archive-only
- intentionally dropped with documented approval

No table is omitted by accident.

Field-level conversions must be documented for financially or operationally significant records.

---

## 8. Target migration order

Dependency order is mandatory. Baseline order:

1. migration run metadata
2. installation/workspace references
3. staff historical identities and active staff mappings
4. roles and location scopes
5. locations
6. categories
7. vendors
8. inventory item master/SKU aliases
9. periods and operational weeks
10. opening balances/state
11. supplier invoices
12. supplier invoice lines
13. inventory receipts
14. inventory pulls/issues
15. counts/adjustments where applicable
16. accepted price history and price observations
17. menu cycles/days/slots/items
18. meal periods
19. HACCP/compliance history
20. checklists/daily operations/incidents
21. events/calendar history
22. certifications/training/equipment where mapped
23. approvals and governed changes
24. business audit history
25. document/file metadata
26. file-object copy and verification
27. final derived-state reconstruction
28. reconciliation
29. rejected/unmapped report
30. acceptance verdict

Children must never be loaded before their resolved target parents unless the loader intentionally stages them and validates before commit.

---

## 9. Historical inventory reconstruction

Inventory history must be reconstructed from historical business events, not replaced with only the latest on-hand quantity.

For each period/item:

ending_quantity = opening_quantity + received_quantity - pulled_quantity

The migration must preserve:
- opening quantity
- opening carried unit cost
- receipts by operational week
- pulls/issues by operational week
- accepted invoice prices
- price review decisions
- closing quantity
- closing value
- period lock/close state
- historical source/invoice linkage

The target CEngine system must independently recompute results and compare against legacy results.

A mismatch beyond an explicitly approved rounding/tolerance fails reconciliation.

---

## 10. Financial and valuation reconciliation

Required checks per period and total:
- opening quantity
- received quantity
- pulled quantity
- ending quantity
- opening value
- received value
- pulled/issued value where applicable
- ending value
- supplier invoice subtotal
- discount
- tax
- fuel/fees
- stated total
- calculated total
- accepted unit price history
- price observation decisions
- approved tolerance/rounding deltas

Money uses decimal arithmetic. Do not reconcile financial values with binary float comparisons.

Reconciliation must preserve the approved historical rule version. A new rule must not silently recalculate closed historical periods differently without a documented migration/recalculation decision.

---

## 11. Historical actor and audit preservation

Migration-generated writes must be distinguishable from the original historical actor.

Required behavior:
- preserve original actor identity where known
- preserve original event timestamp
- preserve original action/event type where meaningful
- include migration provenance
- never claim the migration service itself performed the original business act
- migration system events may record that a historical event was imported, but those are separate from the imported historical event

High-volume HTTP access logs do not need to become business-audit rows merely because they existed in the legacy system. The migration map must explicitly distinguish operational/business audit from transport/access logging.

---

## 12. User and authentication migration

Do not copy plaintext credentials, API keys, service-role keys or reusable secret material.

For staff:
- migrate identity/profile data required for business history
- preserve role and location scope mapping
- historical records continue to resolve to the historical staff identity
- active staff re-enrol credentials according to the new auth contract
- password/PIN resets or first-login enrollment may be required
- authentication secrets are not migration payloads

---

## 13. File and archive migration

Historical files are tenant data and move to the approved MJCC tenant storage destination.

For each object:
1. read the approved source object
2. record source object reference
3. calculate SHA-256
4. record byte size and MIME type
5. copy to the target private bucket
6. calculate destination SHA-256
7. compare hashes
8. create/update the target CEngine document/file metadata record
9. resolve the migrated business-record reference
10. mark verified only after all checks pass

Required reconciliation:
- source object count
- target object count
- byte counts
- hash equality
- no orphan document rows
- no orphan objects that should have business references
- no cross-tenant paths

The existing home archive may remain read-only only for rollback/retention. It is not the permanent production dependency after successful migration.

---

## 14. Idempotency and re-run behavior

The migration program must be safe to re-run.

Required properties:
- importing the same source identity twice does not create duplicate target records
- the same source identity resolves to the same target ID
- migrated child references resolve through the existing ID map
- repeated file copy detects the same hash/object rather than duplicating unnecessarily
- a partially failed run can resume or restart without corrupting successful rows
- cutover delta import can reuse the same mapping/provenance rules
- migration run IDs remain distinct even when source identities resolve to existing target rows

The importer may use upsert semantics only on an explicit migration provenance key. Do not use broad business-field matching that could merge two legitimate records.

---

## 15. Rehearsal procedure

The Wednesday rehearsal follows the authorization in spec/mjcc-backend-complete-by-wednesday.md.

### Before extraction
- verify authorization window is active
- verify legacy reader is read-only
- identify source snapshot/export method
- record source snapshot timestamp
- calculate/record snapshot or export checksum where practical
- record migration code commit
- record target schema identity
- verify target schema is isolated and disposable
- verify no live production writer points to the rehearsal schema

### Extract
- read from snapshot/export or approved read-only source
- never mutate legacy rows
- avoid exporting unnecessary secret material
- encrypt protected artifacts in transit and at rest

### Transform
- normalize using the committed migration map
- resolve IDs deterministically
- validate target types/enums
- produce a rejected/unmapped report
- do not coerce invalid financial data silently

### Load
Preferred path: CEngine API or a migration service using the same target validation/business contracts.

Bulk database loading may be used only for historical structures where direct loading is explicitly reviewed and followed by equivalent validation, provenance and reconciliation. Do not bypass target invariants merely for speed.

### Reconcile
Run all gates in section 18.

### Finish
- record pass/fail
- retain only approved safe summaries in git
- expire/remove rehearsal credentials
- remove real-data rehearsal artifacts according to authorization/retention policy
- rehearsal schema may be destroyed and rebuilt

---

## 16. Final cutover procedure

The final cutover is separately authorized. Rehearsal authorization is not production authorization.

### Prior rehearsals
- full migration rehearsal at least once
- second rehearsal/delta rehearsal where practical
- migration duration measured
- reconciliation proven
- restore/rollback proven

### Cutover window
1. announce migration window
2. stop or disable legacy Compute writes
3. verify no active legacy writers remain
4. take final backup/snapshot
5. record the definitive cutoff timestamp
6. run final full import or rehearsed baseline plus cutoff delta
7. reconcile all mandatory gates
8. run golden business-rule tests
9. verify CEngine API and UI against ce_mjcc_prod
10. issue signed cutover verdict
11. enable CEngine as the only writer
12. keep legacy Compute available read-only

There is no dual-write phase.

### Rollback window

For 14 days:
- CEngine remains the only writer during normal operation
- legacy Compute remains read-only
- daily reconciliation summaries continue
- backups remain available
- rollback procedure remains documented/testable

Any extension must be formally recorded.

---

## 17. Delta migration

The migration tool must support selecting records changed after a recorded snapshot/cutoff when the source provides reliable update/change timestamps or event identity.

Delta rules:
- same source ID maps to same target ID
- inserts create missing target records
- updates apply only where the migration contract allows historical corrections
- deletes are never inferred casually; legacy deletes/tombstones require explicit mapping
- the delta uses the same validation and reconciliation path as a full migration
- the final delta cannot bypass locked-period protections without migration authorization

---

## 18. Mandatory reconciliation gates

No production cutover until all required gates pass.

### Structural
- source table disposition complete
- migration map complete
- required target tables/entities exist
- foreign keys valid
- no orphan required relationships
- legacy-to-target ID map is one-to-one where required
- target IDs stable across re-run

### Row counts
Per mapped table/entity:
- source eligible count
- target migrated count
- rejected count
- archive-only count
- explained difference

Unexplained differences fail.

### Inventory
Per item/period and totals:
- opening
- received
- pulled
- ending
- period/week linkage
- locks/close state

### Valuation
Per item/period and totals:
- opening value
- received value
- ending value
- carried unit cost
- accepted supplier price
- approved rounding tolerance

### Invoices
- invoice count
- invoice-line count
- vendor mapping
- subtotal
- discount
- tax
- fuel/fees
- stated total
- net/calculated total
- duplicate/reference identity

### Price history
- accepted price chronology
- pending/held/reviewed decision mapping
- threshold behavior where applicable
- no history silently rewritten to the latest price

### Menus and operational history
- menu cycle count
- day/slot/item counts
- meal-period count
- events
- HACCP/compliance records
- checklists
- daily logs
- incidents

### Users and permissions
- active staff count
- historical actor resolution
- role ladder mapping
- location scope mapping
- no missing required active user
- no unauthorized escalation introduced by migration

### Audit/history
- required business-audit history count
- historical timestamp preservation
- historical actor preservation
- migration provenance present
- no attribution to the migration service as the original actor

### Files
- object count
- document metadata count
- SHA-256 match
- byte-size match
- business reference resolution
- no orphan required objects
- no cross-tenant object keys

### Period locks
- closed legacy periods remain closed
- locked legacy periods remain locked
- no accidental reopen/recompute

### Rejected/unmapped report
Every rejected or unmapped source row must include:
- source identity
- reason code
- severity
- proposed resolution or explicit accepted disposition

No silent drops.

---

## 19. Migration CI contract

The migration auditor owns CI design and verification. The builder owns implementation. The auditor does not invent product rules.

Normal CI uses synthetic/sanitized fixtures only.

### A. migration-static-check
Verify:
- ordered migration versions
- no duplicate versions
- changed checksum detection
- required migration metadata
- no committed credentials
- no real MJCC customer rows/exports in git
- destructive SQL detection
- mapping documentation present for migration PRs

### B. fresh-install-test
- empty Postgres
- run ce-serve install
- assert expected schema
- run baseline proof

### C. forward-migration-test
- install previous supported schema/version
- apply ce-serve migrate
- assert new schema
- verify migration history
- verify no-op when no migrations pending
- reject checksum mutation
- reject version gap
- reject database version ahead of binary
- reject prohibited destructive migration

### D. migration-fixture-test
- load sanitized legacy fixture
- execute migration
- verify target rows
- verify ID mapping
- verify no orphans

### E. reconciliation-test
Verify golden values for:
- inventory
- valuation
- invoices
- prices
- locks
- user/role mapping
- files using synthetic objects

### F. migration-idempotency-test
Run migration twice and assert:
- no duplicate target records
- target IDs stable
- no duplicate historical events
- no duplicate files
- second run reports replay/no-op correctly

### G. tenant-isolation-test
Create at least two synthetic tenants/data planes and prove:
- tenant A migration cannot read/write B
- tokens/config route to the expected data plane
- missing/invalid tenant DB reference fails closed

### H. schema-security-test
Verify:
- grants
- RLS where applicable
- migration role restrictions
- function search paths
- no unsafe PUBLIC execute
- no legacy mutation grants
- target writer cannot mutate source

### I. migration-performance-test
Record:
- record count
- elapsed time
- rows/sec
- retries
- errors
- lock/wait observations where practical

Initially informational unless a threshold is explicitly approved.

### J. migration-audit-report
Emit safe artifacts:
- migration-audit-report.json
- migration-audit-report.md

Include:
- commit SHA
- migration version
- jobs/tests
- synthetic source counts
- target counts
- reconciliation result
- security checks
- duration
- warnings
- pass/fail

Never attach real customer exports/rows.

---

## 20. Protected real-data rehearsal

Real MJCC data must not be sent to normal GitHub-hosted CI merely to automate rehearsal.

The real-data rehearsal runs only in an authorized protected environment with:
- read-only source access
- rehearsal-only target writer
- short-lived credentials
- encrypted transport
- no raw customer-data artifacts published externally
- safe reconciliation summary only

The auditor may automate orchestration, but the environment must satisfy the written rehearsal authorization.

---

## 21. Auditor responsibilities

The auditor:
- reads this SOP and the Wednesday schedule
- checks implementation against the contract
- creates/maintains migration CI
- verifies fixture quality
- challenges reconciliation logic
- verifies security boundaries
- verifies idempotency
- verifies migration history/checksum behavior
- reviews performance evidence
- reports blockers with evidence
- blocks merge when mandatory gates fail

The auditor does not:
- define new business rules without authority
- silently loosen tolerances
- edit production customer data
- bypass failed reconciliation
- accept missing records because most rows migrated
- upload real customer data into normal CI
- rewrite the target architecture from staging assumptions

---

## 22. Merge blockers

Any of the following blocks a migration PR:
- migration does not compile/run
- fresh install or forward migrate fails
- applied migration checksum can be altered silently
- migration version gap is accepted
- prohibited destructive operation is automatic
- fixture migration loses required data
- unexplained reconciliation mismatch
- duplicate import on re-run
- target ID instability
- cross-tenant read/write
- RLS/grant regression
- real MJCC rows or credentials detected in git/normal CI
- unmapped required source rows without disposition
- file hash mismatch
- locked historical period becomes mutable
- historical actors are re-attributed incorrectly

---

## 23. QUESTIONS_TOPOLOGY handling

QUESTIONS_TOPOLOGY is a planning checklist, not an execution dependency.

The implementation must not pause merely because the question file still contains already-decided questions.

Procedure:
1. Read the current question list if available.
2. Mark questions resolved when the answer is already canonical in this SOP, spec/mjcc-backend-complete-by-wednesday.md, spec/SCHEMA_FREEZE.md, or accepted Loom ADRs.
3. Do not reopen resolved topology decisions during implementation.
4. Leave genuinely unresolved questions open and route them through the documented decision hierarchy.
5. A missing or stale QUESTIONS_TOPOLOGY file does not block work when the relevant decision is already canonical elsewhere.
6. If a topology question conflicts with a newer canonical decision, amend the question record rather than implementing stale text.

Already decided and not blockers:
- production control plane is separate from tenant data planes
- production tenants do not share one CEngine project as the default model
- MJCC production target is MJCCv1 / ce_mjcc_prod
- ce_mjcc_staging is throwaway/test-only
- legacy production remains read-only during rehearsal
- no dual writes at cutover
- Compute remains read-only for the 14-day rollback window
- MJCC UI lives in MJCC-workspace and uses CEngine as the business API
- temporary AI provider adapter is not the permanent provider topology
- old business data is transformed into the new CEngine tables
- archived files move into the approved MJCC tenant storage destination

---

## 24. Definition of done

The migration system is ready for production cutover only when:
- migration map is complete
- migration metadata/provenance exists
- migration tool is repeatable and idempotent
- CI contract is implemented and green
- sanitized fixture migration passes
- forward CEngine schema migration path is proven
- protected real-data rehearsal passes
- all required reconciliation gates pass
- backup/restore passes
- target file migration verifies hashes
- final delta procedure is proven
- one-writer cutover procedure is documented
- rollback procedure is tested
- safe final audit report is generated
- production cutover receives its separate signed authorization

Until then, legacy Compute/MJCC remains the production source of truth.
