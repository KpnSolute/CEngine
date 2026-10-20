# CEngine / MJCC Schema Freeze

**Status:** Canonical architecture freeze for the MJCC migration program  
**Owner:** KpnSolute / CEngine  
**Customer:** Miami Job Corps Center (MJCC)  
**Effective date:** 2026-10-19  
**Applies to:** KpnSolute/CEngine, KpnWorld/MJCC-workspace, MJCC migration/rehearsal work  
**Related documents:**
- `spec/mjcc-backend-complete-by-wednesday.md`
- `spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md`
- `spec/MIGRATION_AUDITOR_BRIEF.md`
- `spec/CLAUDE_MIGRATION_IMPLEMENTATION_PROMPT.md`

---

## 1. Purpose

This document freezes the major database and ownership architecture before implementation continues.

The goal is not to prohibit all schema change. The goal is to stop foundational ownership, tenancy, identity, migration and audit decisions from moving underneath the builders.

"Frozen" means the major boundaries and relationships in this document are treated as settled for the current implementation window unless a later accepted architecture decision explicitly supersedes them.

Additive, backward-compatible columns, indexes, constraints, helper tables and implementation details may still change through the reviewed migration process.

Breaking changes do not silently redefine this freeze.

---

## 2. Production topology

### 2.1 Control plane

CEngine has one KpnSolute-controlled control plane.

The control plane owns platform-level records such as:

- installation registry
- customer/installation identifiers
- module catalog
- template catalog
- plugin approval records
- plugin/module version metadata
- approved hashes/digests
- deployment and release metadata
- capability grants
- routing metadata
- billing and entitlement references
- aggregated usage/metering
- control-plane audit events

The control plane does **not** become the default location for customer business data.

### 2.2 Tenant data planes

Each production customer gets a dedicated authorized tenant data environment.

The production target is **not** one CEngine project with one schema per customer.

Shared schemas/projects may be used for:

- local development
- CI
- synthetic testing
- temporary staging
- disposable rehearsal environments

They are not the default production tenancy model.

### 2.3 MJCC

MJCC's production tenant data plane is:

- Supabase project: `MJCCv1`
- project ref: `mgvyylvmkxhhataavqjz`
- organization: JobCorp
- production CEngine schema: `ce_mjcc_prod`

The temporary staging environment is:

- project ref: `aqkhebljkagytsqtkkly`
- schema: `ce_mjcc_staging`

`ce_mjcc_staging` is explicitly throwaway/test-only.

No staging identifier, staging default or temporary control-plane row may be treated as production truth.

---

## 3. Data ownership boundary

### 3.1 Tenant data stays in the tenant data plane

The following remain with the customer tenant data plane:

- business transactions
- inventory records
- invoices
- supplier data
- menus
- HACCP/compliance records
- daily logs
- incidents
- checklists
- users/staff business identities
- business audit history
- notifications
- jobs
- idempotency records
- files
- document metadata
- backups
- migration provenance for customer business records
- tenant-specific operational history

### 3.2 Control-plane records stay central

Examples:

- installation created
- template version changed
- plugin approved
- plugin digest changed
- installation deployed
- installation disabled
- usage aggregate incremented
- entitlement changed
- tenant data-plane reference updated

Customer payloads are not copied into the control plane merely for observability.

Metering stored centrally must be minimal and aggregated unless a later approved contract explicitly requires otherwise.

---

## 4. Schema ownership

### 4.1 Control-plane schema

The control-plane schema contains only platform/control records.

It must not contain operational copies of MJCC inventory, invoices, audit history, jobs, notifications or files.

### 4.2 Tenant installation schema

Each CEngine installation has its own target schema inside its tenant data plane.

For MJCC production:

`ce_mjcc_prod`

The schema owns:

- runtime-managed entity tables
- tenant business records
- installation migration history
- idempotency state
- tenant jobs/outbox
- tenant audit/events
- tenant staff records when the selected auth adapter stores them there
- migration provenance for imported business records

---

## 5. Installation identifiers

The following identifiers are distinct and must not be conflated:

- **customer id**: commercial/customer identity
- **tenant id**: authorization/data-isolation identity
- **installation id**: one installed CEngine workspace/runtime instance
- **schema name**: physical Postgres schema for an installation
- **workspace/public slug**: human/product routing identifier where used
- **plugin id/version/digest**: executable policy identity
- **template id/version/hash**: reusable structure identity

No API caller chooses its own tenant by sending an arbitrary tenant id.

Tenant scope must be resolved from trusted identity/control-plane configuration.

---

## 6. Request routing

A production request resolves in this order:

1. authenticate caller
2. resolve installation
3. resolve trusted tenant scope
4. resolve tenant data-plane connection/reference
5. verify installed template/module/plugin revision
6. authorize requested action
7. execute against the tenant data plane
8. audit business action in the tenant data plane
9. emit only approved control-plane usage/operational metadata centrally

If an installation has no valid tenant data-plane reference, the request fails closed.

No fallback to another customer's schema is permitted.

---

## 7. Auth subject model

Identity and business authorization are separate concerns.

Long-term target:

- Platform owns product identity/session issuance
- CEngine verifies a scoped token/assertion
- CEngine derives the allowed tenant/installation scope from trusted configuration
- business roles/scopes come from the installed manifest/runtime contract

For MJCC during migration, an adapter may bridge current auth into a short-lived CEngine token.

The caller never selects the tenant.

Auth secrets are not migration payloads.

Historical staff identities may be migrated for attribution, but passwords, PINs, API keys and service-role secrets are not copied as business data.

---

## 8. Table families

The following table families are frozen conceptually.

Exact table names may follow CEngine conventions, but responsibility does not move between families without an approved architecture change.

### 8.1 Engine/runtime metadata

Examples:

- installation schema version/migrations
- manifest/template hash
- plugin/module revision references
- runtime configuration metadata

### 8.2 Business records

Examples:

- inventory
- vendors
- invoices
- menus
- HACCP
- checklists
- logs
- incidents
- tasks
- calendar/events
- certifications
- equipment
- budgets
- purchase/receiving records

### 8.3 Audit/events

Tenant business audit is separate from:

- HTTP/access logs
- control-plane audit
- deployment logs

Do not flood the business audit table with transport/access telemetry.

### 8.4 Jobs/outbox

Tenant jobs, retry state, outbox events and delivery/idempotency state live with the tenant installation unless an explicitly centralized platform service owns only the orchestration metadata.

Customer payloads remain tenant-scoped.

### 8.5 Migration/provenance

Required concepts:

- migration run
- source snapshot identity
- legacy-to-target ID mapping
- reconciliation results
- migration errors
- safe artifact metadata

These live with the tenant migration/installation context.

---

## 9. Migration/versioning model

The installation schema carries its own migration history.

CEngine must support a reviewed forward-migration path.

Current freeze:

- forward-only
- versioned
- ordered
- checksum-pinned
- additive/backward-compatible changes first
- plan/dry-run where practical
- no silent best-effort migration
- no automatic destructive migration during this implementation window
- fresh install + migrate path must be tested
- no-op when already current
- fail if migration order is invalid
- fail if an already-applied migration checksum changes
- fail if database version is ahead of the binary

The migration-history contract belongs to the tenant installation schema.

The control plane may maintain its own independent migration history for control-plane schema changes.

---

## 10. Idempotency model

Idempotency is a first-class runtime contract.

For consequential write operations:

- requests use an explicit idempotency key or deterministic migration provenance key
- replay returns or resolves the original result
- a different payload must not silently reuse another request's result
- migration re-runs do not duplicate imported business records
- file migration re-runs do not duplicate already-verified content unnecessarily
- jobs/deliveries must be safely retryable

Migration idempotency uses explicit source provenance, not loose business-field matching.

---

## 11. Audit/event model

### 11.1 Tenant business audit

Business audit records stay in the tenant data plane.

They preserve:

- actor
- action/event type
- timestamp
- affected resource
- before/after or safe change summary
- plugin/template/manifest provenance where relevant
- request/correlation id
- migration provenance when imported

### 11.2 Historical migration

Historical actors and timestamps remain historical.

The migration service must not become the apparent original actor for imported business events.

Migration-system events are separate from imported historical business events.

### 11.3 Control-plane audit

Control-plane audit covers:

- install
- deployment
- approval
- version/digest change
- entitlement change
- routing/data-plane reference change
- platform administrative action

It does not duplicate tenant business payloads.

---

## 12. Jobs/outbox model

Tenant-scoped jobs and outbox records must include enough identity to guarantee:

- tenant isolation
- retry safety
- lease/claim safety
- dead-letter handling
- auditability
- idempotent side effects

Jobs may never resolve their tenant from untrusted job payload data alone.

---

## 13. File metadata and blob layout

Files are tenant data.

The target model separates:

- business/document metadata
- object/blob storage reference
- content hash
- size
- MIME type
- tenant prefix/path
- source/migration provenance where applicable

Production MJCC files belong in the approved MJCC tenant-controlled storage destination.

The home archive is migration source/rollback material, not the permanent production dependency.

Cross-tenant object keys are prohibited.

---

## 14. Plugin/template references

Consequential business results must be attributable to the rules that produced them.

Where relevant, records/audit should be able to resolve:

- template version/hash
- module version
- plugin id
- plugin version
- plugin digest
- effective policy/rule revision

A plugin upgrade must not silently rewrite closed historical results.

---

## 15. Historical data migration contract

Old MJCC data is transformed into the new CEngine tables.

It is not reduced to:

- only current balances
- only latest prices
- only latest user state
- external legacy references with no migrated history

Historical migration must preserve the business chain required by:

- inventory periods
- receipts
- pulls/issues
- invoice history
- accepted prices
- approvals
- locks
- menu history
- compliance/operations history
- actor attribution
- file references

The canonical detailed contract is:

`spec/MJCC_HISTORICAL_DATA_MIGRATION_SOP.md`

---

## 16. One-writer cutover

The write authority model is frozen:

Before cutover:
- Compute = writer
- CEngine = rehearsal/validation

At cutover:
- legacy writes stop
- final backup/snapshot
- final migration/delta
- reconciliation
- signed cutover decision

After cutover:
- CEngine = only writer
- Compute = read-only fallback

No dual writes.

Compute remains read-only for a 14-day rollback/reconciliation window unless a formal extension is recorded.

---

## 17. Real-data rehearsal boundary

The authorized Wednesday rehearsal:

- reads a snapshot/export or approved read-only source
- never mutates the live legacy schema
- writes only to the isolated rehearsal schema
- uses separate source-reader and target-writer credentials
- records source snapshot time/checksum
- records the migration run
- commits only safe counts/totals/hashes, never customer rows
- expires/removes rehearsal credentials at the end of the authorized window

This does not authorize production cutover.

---

## 18. Temporary control plane

A dedicated permanent CEngine control-plane project has not yet been created.

Until one exists:

- the staging/testing environment may temporarily hold control-plane records
- every such record must be marked temporary
- no new Supabase project may be created without explicit owner approval
- temporary control-plane state is not production truth
- later migration into the permanent control plane must be reconciled, not assumed

---

## 19. What may still change without reopening the freeze

The following may evolve through normal reviewed migrations:

- additive columns
- indexes
- non-breaking constraints
- helper tables
- implementation-specific names
- performance optimizations
- derived views
- safe metadata fields
- batch/backfill mechanics
- CI implementation
- code organization

They must remain consistent with the frozen boundaries above.

---

## 20. What requires an architecture decision

The following are not ordinary implementation details:

- moving customer business data into the control plane
- switching production tenancy to shared schemas in one project
- allowing dual writes
- changing MJCC's production data plane away from MJCCv1 without a documented decision
- changing the 14-day rollback model
- centralizing tenant business audit payloads
- making Platform a mandatory proxy for all CEngine business API traffic
- removing lineage/idempotency requirements
- automatically applying destructive migrations
- changing customer-file ownership/storage boundary
- changing who owns auth identity versus CEngine authorization
- changing plugin/template/customer ownership boundaries

These require a recorded architecture/governance decision, not an incidental code change.

---

## 21. Builder rule

Builders must read this file before major schema or migration work.

If implementation reveals a missing detail:

1. preserve the frozen ownership/security boundaries;
2. choose the safest reversible implementation;
3. document the decision;
4. amend this file only if the clarification changes the canonical schema contract;
5. continue without reopening already-decided topology.

Do not use missing detail as an excuse to improvise a new tenancy architecture.

---

## 22. Auditor rule

The migration auditor verifies code and CI against this freeze.

The auditor must reject:

- cross-tenant schema access
- customer data in the control plane
- unstable migration identity
- migration checksum mutation
- silent data loss
- hidden dual-write assumptions
- migration roles that can mutate the source
- target writers that can cross tenant boundaries
- file paths without tenant isolation
- historical records losing actor/time provenance
- staging-only assumptions leaking into production design

---

## 23. Acceptance checklist

Before schema-dependent implementation is considered aligned:

- [ ] control-plane vs tenant-data-plane boundary preserved
- [ ] MJCC production target = MJCCv1 / ce_mjcc_prod
- [ ] staging explicitly test-only
- [ ] tenant resolution fails closed
- [ ] installation migration history defined
- [ ] idempotency model defined
- [ ] audit/event separation defined
- [ ] jobs/outbox tenant scope defined
- [ ] file metadata/storage boundary defined
- [ ] plugin/template provenance defined
- [ ] auth subject model defined
- [ ] historical migration provenance defined
- [ ] one-writer cutover preserved
- [ ] real-data rehearsal boundary preserved
- [ ] temporary control-plane rows marked temporary

This document satisfies the Monday schema-freeze requirement in spec/mjcc-backend-complete-by-wednesday.md.
