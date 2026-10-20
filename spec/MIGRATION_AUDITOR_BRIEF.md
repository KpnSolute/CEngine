# Migration Auditor Brief: build CI and produce the Claude Code implementation handoff

**Role:** independent migration auditor and CI owner
**Repository:** KpnSolute/CEngine
**Primary contract:** spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md
**Schedule:** spec/mjcc-backend-complete-by-wednesday.md
**Builder:** Claude Code
**Objective:** turn the canonical migration SOP into enforceable CI gates, then produce a precise implementation prompt for Claude Code.

---

## 1. Operating rule

You are the auditor, not the product architect and not the migration builder.

The migration SOP is the contract. Do not invent new migration rules, weaken reconciliation requirements, relax tenant isolation, or redefine the production topology.

Your job is to:

1. read the canonical SOP and schedule;
2. inspect the current CEngine and MJCC-workspace implementation;
3. identify which migration controls already exist and which are missing;
4. create CI that proves the contract using synthetic/sanitized fixtures;
5. generate a detailed implementation handoff prompt for Claude Code;
6. review Claude's resulting work against the SOP and CI gates.

Do not use real MJCC rows in normal CI.

---

## 2. Does QUESTIONS_TOPOLOGY block this work?

No.

QUESTIONS_TOPOLOGY is a planning checklist, not an implementation prerequisite.

Use it only if the file is available in the working tree or supplied by the planner. Do not stop to ask the owner for it.

When it is available:

- mark already-decided questions as resolved by reference to the canonical source;
- do not reopen decisions already fixed by the migration SOP or Wednesday schedule;
- leave only genuinely unresolved questions open;
- if the question file conflicts with a newer canonical document, update the question status rather than implementing stale wording.

The following topology decisions are already resolved and must not block the migration program:

- CEngine control plane is separate from tenant data planes.
- Each production customer gets its own dedicated data environment.
- MJCC production data remains in MJCCv1.
- Production target schema is ce_mjcc_prod.
- ce_mjcc_staging is test-only and throwaway.
- Customer business records do not live in the CEngine control-plane database.
- Real-data rehearsal is read-only at the source and isolated at the target.
- There is no dual-write cutover phase.
- Compute becomes read-only at cutover and remains fallback for 14 days.
- MJCC UI lives in MJCC-workspace and CEngine is the business API.
- Historical data is transformed into the new CEngine tables, not merely linked or archived.
- Archived customer files move to tenant-controlled storage.
- AI provider binding is an implementation adapter, not part of the template's permanent provider topology.

If no QUESTIONS_TOPOLOGY file is present, record that fact once and continue.

---

## 3. Read before doing anything

Read, in this order:

1. spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md
2. spec/mjcc-backend-complete-by-wednesday.md
3. spec/SCHEMA_FREEZE.md if present
4. deploy/render/STAGING.md
5. spec/migrations.md if it exists
6. existing CEngine migration/install code
7. MJCC-workspace migration tooling and docs if present
8. current GitHub Actions workflows in both repos
9. current tests around install, schema creation, tenant isolation, inventory parity, files and idempotency

Preserve unrelated worktree changes.

---

## 4. First deliverable: migration audit matrix

Create or update:

docs/MIGRATION_AUDIT_MATRIX.md

For every major SOP requirement, record:

| Contract | Current implementation | Test evidence | CI gate | Status | Gap owner |
|---|---|---|---|---|---|

At minimum include:

- migration metadata/provenance
- source-to-target ID mapping
- forward schema migration/versioning
- checksum immutability
- migration ordering
- destructive-operation refusal
- fresh install
- previous-version-to-current migration
- fixture ETL
- inventory reconciliation
- valuation reconciliation
- invoice reconciliation
- price-history preservation
- role/user mapping
- audit-history attribution
- file hash verification
- period locks
- idempotent re-run
- rejected/unmapped report
- tenant isolation
- source read-only privilege
- target writer privilege boundary
- no real data in normal CI
- safe audit artifact generation
- backup/restore
- final delta behavior

Do not mark a requirement complete because a document claims it. Require executable evidence.

---

## 5. Second deliverable: migration CI

Create the migration CI implementation, preferably as:

.github/workflows/migration-ci.yml

You may split reusable scripts under scripts/ci/ or tests/migration/ if that keeps the workflow readable.

Required jobs:

### migration-static-check

Must fail on:
- duplicate migration version
- changed checksum for an applied fixture migration
- prohibited destructive SQL in the protected migration set
- obvious credential material
- real MJCC exports or customer-row fixtures committed to the repository
- missing required migration mapping/documentation for migration changes

### fresh-install-test

- create clean Postgres
- run current install path
- assert required base schema
- run baseline CEngine proof
- save only safe logs

### forward-migration-test

- install a previous supported schema fixture
- run ce-serve migrate
- assert target schema/version
- verify migration history
- verify no-op when current
- verify changed checksum fails
- verify version gap fails
- verify target schema ahead of binary fails
- verify prohibited destructive migration fails

If ce-serve migrate does not yet exist, build the CI skeleton so the job is an explicit expected blocker and make that requirement prominent in the Claude handoff.

### migration-fixture-test

Use synthetic/sanitized legacy-shaped fixtures only.

Assert:
- migration completes
- required target records exist
- ID map resolves every required relation
- no unexplained orphan
- rejected records are explicit

### reconciliation-test

Use golden expected results for:
- inventory openings
- receipts
- pulls
- ending quantity
- valuation
- invoices
- accepted price history
- period locks
- role/user mapping
- synthetic file hashes

### migration-idempotency-test

Run the same import twice and prove:
- stable target IDs
- no duplicate business records
- no duplicate historical events
- no duplicate files
- replay/no-op behavior is explicit

### tenant-isolation-test

At minimum two synthetic tenants/data planes.

Prove:
- A cannot read B
- A cannot write B
- target routing resolves the correct tenant DB reference
- missing/invalid tenant reference fails closed

### schema-security-test

Verify:
- migration reader is read-only in the test model
- target writer cannot mutate source
- RLS/grants are correct where applicable
- SECURITY DEFINER helpers use safe search_path if present
- no unintended PUBLIC execute
- no cross-schema privilege leak

### migration-performance-test

Record:
- row counts
- elapsed time
- rows/sec
- retries/errors
- obvious lock/wait regressions

Do not invent a hard pass threshold unless the SOP or existing performance contract defines one.

### migration-audit-report

Generate:
- migration-audit-report.md
- migration-audit-report.json

The artifacts may contain counts, totals, hashes, statuses and test evidence. They must not contain customer rows, credentials or secret values.

---

## 6. Third deliverable: prompt for Claude Code

After the audit matrix and CI skeleton exist, create:

spec/CLAUDE_MIGRATION_IMPLEMENTATION_PROMPT.md

This is the prompt the owner will hand directly to Claude Code.

It must be implementation-ready, not motivational prose.

The prompt must include:

### Authority

State that Claude must follow:
1. MJCC_HISTORICAL_DATA_MIGRATION_SOP.md
2. mjcc-backend-complete-by-wednesday.md
3. SCHEMA_FREEZE.md
4. accepted repository governance

Tell Claude not to ask the owner ordinary implementation questions. The SOP is the companion. When implementation exposes an ambiguity that can be resolved from those sources, make the safest reversible decision, document it, amend the relevant SOP if needed, and continue.

### Exact mission

Claude is the primary builder for:
- ce-serve migrate
- migration metadata tables/contracts
- MJCC source-to-target ETL
- provenance and legacy ID mapping
- reconciliation engine
- protected real-data rehearsal tooling
- file-copy verification
- delta migration
- migration documentation
- required application/domain integration

### Exact repository boundaries

Clarify:
- generic migration/runtime capabilities belong in KpnSolute/CEngine
- MJCC-specific mappings/ETL/goldens belong in KpnWorld/MJCC-workspace
- legacy KpnSolute/Compute is read-only source/reference
- no private customer rows enter CEngine source control

### Non-negotiable implementation properties

Require:
- forward migration versions
- applied checksum verification
- additive/backward-compatible first
- no automatic destructive migration this week
- plan/dry-run support
- idempotent ETL
- stable legacy-to-target map
- rejected/unmapped report
- decimal financial comparisons
- historical actor preservation
- period-lock preservation
- file SHA-256 verification
- one-writer cutover design
- 14-day rollback design
- no real data in normal CI

### Required mapping coverage

Prompt Claude to map and implement at least:
- inventory items/SKUs/categories/vendors
- periods and weeks
- opening balances
- receipts/pulls/movements
- invoices/lines
- accepted prices/price reviews
- menu structures
- HACCP/compliance
- logs/checklists/incidents/events
- staff/roles/location scopes
- approvals/business audit
- files/documents
- explicitly approved AI configuration metadata without secrets

### Required tests

Claude must make every auditor CI gate green.

The prompt must name each workflow job and instruct Claude to treat failing gates as contract failures, not CI annoyances.

### Real-data rule

Claude may not place real MJCC rows in git or normal CI.

The protected Wednesday rehearsal may only run under the authorization already recorded in the schedule, with:
- read-only source
- isolated target writer
- source snapshot time/checksum
- short-lived credentials
- safe reconciliation summary
- credential expiry after the window

### Deliverable report

Require Claude to return:
- commits
- files changed
- migrations added
- commands/tests run
- CI run links or exact local results
- reconciliation coverage
- remaining blockers
- whether any real-data rehearsal was run
- whether anything live was changed
- rollback state

Do not allow "done" without evidence.

---

## 7. Auditor review after Claude works

When Claude returns implementation:

1. inspect the actual diff;
2. run or inspect every migration CI gate;
3. compare implementation against the audit matrix;
4. identify any undocumented source table or target entity;
5. verify no real customer data or secrets entered git;
6. verify no silent tolerance change;
7. verify no dual-write assumption;
8. verify no migration bypasses CEngine invariants without explicit reviewed reason;
9. verify rejected/unmapped rows fail safely;
10. publish a PASS / CONDITIONAL / FAIL verdict with exact evidence.

Do not repair Claude's implementation silently while auditing. File precise defects or bounded follow-up tasks. If a trivial CI-only fix is required, isolate and document it.

---

## 8. Merge authority

The auditor may block the migration implementation from merge for any SOP merge blocker.

The auditor must not approve production cutover. Production cutover remains a separate signed owner decision after rehearsal, reconciliation, backup/restore and rollback proof.

---

## 9. Expected immediate sequence

1. Read canonical migration SOP.
2. Read Wednesday schedule.
3. Check for QUESTIONS_TOPOLOGY; use it only as a resolution checklist.
4. Produce migration audit matrix.
5. Build migration CI skeleton/jobs.
6. Produce the Claude Code implementation prompt.
7. Hand the prompt to Claude.
8. Audit Claude's branch/PR.
9. Keep CI red until the implementation truly satisfies the contract.

Do not wait for QUESTIONS_TOPOLOGY before starting steps 4-6.
