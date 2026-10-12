# CEngine Modular Runtime and MJCC First-Tenant Integration

Status: **Architecture proposal for owner/Loom review, not an approved contract or implementation claim.**
Date: 2026-10-12. Audience: Claude, Gemini, Codex, Compute and CEngine maintainers.
Related: `README.md`, `spec/groups-and-connectors.md`, `spec/capability-map.md`, `spec/manifest.schema.json`, and KpnSolute/Compute. Preserve the approved ADR-0007 separation of responsibilities; route material changes through Loom change control before implementation.

## Decision being explored

CEngine is a general-purpose **business execution runtime**, not a dashboard-only engine and not 15 hardcoded industry applications. Think of Python's core plus importable libraries such as `discord.py`:

- **Core**: typed records, tenant isolation, authorization, state machines, transactions, validation, audit, changesets, events, period semantics, file abstractions, query planning and deterministic execution.
- **Modules**: reusable packages of business capabilities. Example names: `inventory`, `approval`, `calendar`, `contacts`, `reporting`, `documents`, `billing`, `people`. A module supplies typed operations, interfaces, capability requirements, versioned contracts and optional rule extension points. A module is not one tenant's data or a separate hosted service by default.
- **Business rules/policies**: configuration and constrained, declarative expressions determining a tenant's actual calculations, validations, transitions and approvals. Prefer this over code execution for ordinary variations.
- **Extensions/plugins**: optional, reviewed and versioned implementations of policy interfaces for behavior that cannot be expressed in the rule language. Sandbox code if arbitrary execution is eventually allowed; explicitly constrain I/O, CPU, memory, time, versions, permissions and side effects. WASM is a candidate, not a decided dependency.
- **Template**: platform-owned, versioned and validated *skeleton* declaring its artifacts, entity shapes, imported modules, module versions, workflows, policies, views, roles and connector capabilities. It contains **no customer business records, credentials or confidential SOP content**. The library/registry is stored in KpnSolute-controlled systems.
- **Instance**: tenant-specific installation/configuration of a template, with bindings, private SOPs, selected versions and operational data isolated to the customer's authorized data environment.
- **Compute**: the customer-facing persona, AI chat, artifact editor, templates/gallery, review, generation and workspace interface. AI-generated proposals are never implicitly authorized executable actions.
- **Infajn**: resource provisioning and provider I/O (including connectors, storage, compute and model resources). CEngine controls access, invokes approved capabilities, validates results, enforces policy, meters and audits. CEngine must not silently call provider APIs itself.

These terms distinguish an **importable capability** from a **template that selects capabilities** and from a **connector that accesses external infrastructure**. Modules compose across all 15 documented business groups; category lists express supported use cases, not a requirement for a bespoke engine per industry.

## Proposed template/module contract (illustrative, NOT valid against current manifest schema)

```json
{
  "template": {"name": "food_service_operations", "version": "0.1.0"},
  "imports": [
    {"module": "core.records", "version": "1.x"},
    {"module": "business.inventory", "version": "1.x"},
    {"module": "business.approvals", "version": "1.x"},
    {"module": "business.periods", "version": "1.x"}
  ],
  "policies": {
    "inventory.ending_quantity": {"rule": "max(0, opening + received - pulled)", "version": 1},
    "inventory.valuation": {"implementation": "foodservice.mjcc_valuation", "version": "1.0.0"},
    "price_review": {"threshold_fraction": 0.25}
  },
  "required_connectors": ["storage.files", "ai.document_extraction"]
}
```

The current `spec/manifest.schema.json` does **not** support this `imports` contract; design, validate and version the schema change before accepting such manifests. Connector entries are **capability types**, not vendor credentials or hostnames. Resolve imports through a locked dependency graph; deny unknown, conflicting or unapproved versions. Generated templates must remain declarative and signed/hash-pinned after approval.

## Policy evaluation and compatibility rules

1. **Three tiers**: (A) parameters/configuration, (B) pure typed expressions and state/action policies, (C) explicitly approved extensions implementing stable host interfaces.
2. **Deterministic inputs/outputs**: declare input/output types, currency and rounding rules, null handling, time zone/clock rules, effective dates and required context. Avoid float math for monetary amounts; use checked decimal arithmetic.
3. **Transactional integrity**: rules in a write path must participate in the same durable transaction or have a documented compensating workflow. Side effects go through an outbox and Infajn-managed connectors, with idempotency keys.
4. **Immutable history**: pin the exact module/policy/template versions used for each consequential result. Rule upgrades never silently recalculate closed periods, posted invoices or payroll. Require a migration/recalculation plan, approval and audit.
5. **Isolation**: never trust tenant IDs from caller-provided manifest content. Verify authorization at boundary and inside data access; keep confidential records, customer-specific SOPs and credentials out of the shared template registry.
6. **Governance**: require review of new modules, plugin permissions and template publication; enforce explicit approval for money movement or external writes. Include provenance, license, security checks and rollback behavior.
7. **Conformance**: test policy implementations against language-neutral vectors, tenant-isolation tests, resource ceilings, golden business cases and replayability before registration.

## MJCC is the first tenant and reference template

MJCC is a real Miami Job Corps food-service operation, **not the universal data model**. Extract its *behavioral structure* from KpnSolute/Compute without copying tenant data into KpnSolute's shared template registry. Preserve the existing running system during parallel validation.

Coverage to model or adapt:
- SKU catalog, vendors, inventory received/pulled per operational week, month opening/close/rollover.
- Exact quantity and valuation rules, including carried opening unit cost; row-level decimal rounding; latest accepted invoice prices; 25% price-review gate and drift checks; over-pull audit signals.
- Invoice upload, PDF/OCR extraction, staged review, manager approvals and source-control/changeset history.
- Menus and cycles, HACCP temperatures, daily logs, checklists, events, reports, printable forms and permissions.
- Staff/manager/admin roles, existing identity flow, audit history, historical data and tenant boundaries.

Potential boundary:
- **Template**: field definitions, formulas, role/action definitions, UI artifacts, connector capability requirements and sample *synthetic* fixtures.
- **CEngine**: execute the validated policies and record lifecycle; enforce access, audit, close/lock behavior and version semantics.
- **Compute**: preserve MJCC-facing interactive pages while gradually connecting the data/actions to CEngine.
- **Infajn**: future provider-level execution (object storage, document extraction, AI and other connector provisioning); use explicit temporary adapters where the provider integration is not yet ready.
- **MJCC data**: retained separately; production remains source of truth until a reconciled cutover.

Existing `spec/examples/inventory.json` is a narrow demonstration, NOT a complete MJCC template. It uses simplified `on_hand * unit_cost` valuation and does not capture the current MJCC inventory accounting rules. Do not treat it as a migration contract.

## Near-term execution plan: coming weeks, not 2027

**Phase 0: inventory and freeze contracts (days 1–3)**
- Audit current CEngine crates, executed tests, CI, deployment status and Compute API/database contracts; record evidence per capability.
- Gather MJCC golden cases with sanitized inputs and expected outputs: receipt, issue, over-pull, multiweek totals, price changes, price-review accept/reject, month rollover, invoice reconciliation, report totals and role guards.
- Freeze current production baseline, backup/recovery and rollback procedure. **No production data migration.**

**Phase 1: make MJCC expressible (week 1)**
- Map each MJCC action to: built-in runtime capability, declarative policy, missing module, or Infajn/temporary adapter. Maintain a blocker ledger.
- Define and schema-validate MJCC manifest skeleton; specify any versioned imports/module interface proposal separately from currently supported v1 manifest.
- Add conformance fixtures proving decimal rules and period invariants.

**Phase 2: parallel execution (week 2)**
- Install a separate CEngine test instance with test/sanitized MJCC data, keeping original Compute production unchanged.
- Exercise auth, tenant isolation, inventory calculations, review/merge, period close, audit, reports and failure/retry paths.
- Create an adapter from existing Compute UX to CEngine's proven API, one vertical slice at a time.

**Phase 3: reference tenant gate (weeks 3–4, contingent)**
- Validate SOP/document and AI connector flows through limited adapters or Infajn where available; confirm all provider access is explicitly permissioned.
- Demonstrate repeatable installation of the same template skeleton in a *second isolated synthetic tenant* without copying any MJCC private data.
- Reconcile numerical outputs, functional behavior, authorization, auditing and rollback. Only propose production cutover after written acceptance and proven recovery. If failures remain, extend parallel operation rather than rush a date.

## Acceptance gates, not aspirations

- MJCC golden fixtures produce identical quantity, valuation, rounding and close results through CEngine as through the existing approved business rules.
- Privileged and unprivileged roles cannot exceed their manifest-granted access; a second tenant cannot read or write MJCC records, files or audit data.
- Changeset approvals, immutable period history, version pinning and replay/idempotency are demonstrated.
- The same published *skeleton* can initialize a second tenant against distinct data without changes to core runtime or leaked private content.
- Connector calls and consequential writes are explicit, audited, bounded and recoverable; no credentials in manifests.
- Compute UI remains usable or is intentionally feature-gated; migration has tested backup, restore and rollback.
- All evidence comes from executed tests and deployment checks, **not** existence of source files alone.

## Immediate engineering guidance for Claude

1. Read this as a **review proposal**. Check ADR-0007 and Loom product change control before treating any new import/plugin API or architecture change as approved.
2. Do not rewrite the MJCC application or replace the production Supabase schema prematurely.
3. Compare the actual CEngine implementation against this proposal; correct stale README claims separately with evidence.
4. Start from existing modules/crates and published conformance vectors. Implement the smallest safe vertical slice for the first MJCC tenant, then expand.
5. Keep a single blockers/test-results register in the repo's permitted documentation location; include exact file paths, test commands, outputs, scope and owner approvals.
6. Do not interpret this document as permission to deploy, incur costs, alter customer data, or merge without review.
