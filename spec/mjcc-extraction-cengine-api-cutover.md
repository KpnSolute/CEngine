# MJCC Extraction and CEngine Plugin API Cutover: Final Target Architecture

**Status:** Owner-directed target architecture, implementation specification pending Loom reconciliation. No code migration, data move, deploy or cutover authorized by this document.
**Date:** 2026-10-12
**Read after:** [PR #48 modular CEngine](./modular-runtime-mjcc-integration.md), [PR #50 Persona pipeline](./persona-private-plugins-pipeline.md), [PR #52 customer GitHub lifecycle](./customer-github-plugin-lifecycle.md). This document captures subsequent explicit owner clarification and replaces contrary *proposals* in those earlier documents. Accepted Loom ADRs have higher authority.

## Non-negotiable direction

**Two coordinated deliverables, not a choice between them:**
1. Finish the reusable **CEngine plugin execution system**: CEngine loads a version-pinned customer-specific plugin, runs its rules and workflows, and owns the public **business execution API**.
2. Extract **MJCC**, the independent Miami Job Corps Center customer tenant/application currently mixed into `KpnSolute/Compute`, into a separate, maintained, customer-authorized repository. Its own UI, configuration, tests and private plugin source live there. **MJCC does not continue to use a separate MJCC business API as its primary backend. It uses CEngine's API.** The CEngine runtime and generic modules remain shared, not copied into MJCC.

Use this work to prove **Template #1**, a generalized, reusable food-service template *derived from the application structure*, never from customer-private SOPs or operational data. The shared Templates V1 registry/repository, Persona automation, Infajn expansion and full KpnSolute Platform are **later work** and must not block this immediate first tenant.

## Desired execution topology

```text
  MJCC's own authorized private GitHub repository
  - MJCC UI / assets / app integration layer
  - workspace-specific declarative configuration
  - MJCC plugin source, version descriptors and tests
  - deployment/build definitions (NO credentials or operational records)
              |
       pinned plugin build/manifest   ---> isolated build + test + approval
              |                                     |
              +-------------------------------------+
              |
              v
        CEngine (shared runtime and HTTP API)
        - universal HTTP endpoints for entities/actions/workflows
        - shared reusable modules
        - policy/plugin loader and safe execution host
        - authorization, tenant isolation, versions, transactions,
          invariants, audit, changesets, idempotency and events
              |
              v
        MJCC's authorized data environment
        (private inventory, menus, logs, people, invoices, history)

  MJCC UI --> CEngine API --> module --> MJCC private plugin
                                        (rules execute inside defined host)
```

**No hidden forwarding of substantive MJCC business operations to old FastAPI routes** as proof of CEngine execution. Temporary migration-only adapters must be separately named, scoped, measured and retired; production parity is required before removal of working functionality. CEngine may use its own adapters internally but must remain the actual decision/execution authority.

## What belongs where

| Component | Ownership/responsibility |
| --- | --- |
| CEngine repo | Generic execution host, API, loader, reusable module and capability contracts, conformance suite |
| MJCC repo | Tenant-specific UI, bindings/config, **private plugins**, golden tests and deployment references |
| Tenant data store | Real MJCC transactions, SOPs where authorized, user records, files, backups |
| Generalized Template #1 | Reusable KpnSolute-owned food-service structure; **no private MJCC plugin code** or tenant data |
| KpnCompute repo | Ultimately platform/generation and management code; phased removal of MJCC-specific implementation |
| Persona | **Not needed** to create initial plugins; eventually generates/updates them **only on user request** |
| Customer GitHub authorization | Eventually customer connects their own GitHub through least-privilege integration; for MJCC, resolve a specifically authorized customer repository before writes |

**GitHub is private plugin *source control*, not a production database or on-request code execution source.** Build pinned artifact, verify signature/digest, retain rollback, and cache/deploy independently of GitHub uptime. No secret keys in commits.

## Required plugin host and workflow contracts

Before implementing new modules/plugins, define:
- module IDs, versions, typed operations, extension hooks, dependency resolution and compatibility;
- plugin manifest (tenant/workspace binding, allowed module/hooks, version, hash, inputs/outputs, capability grants, side-effect policy, resource limits);
- controlled host execution (declarative rules wherever practical, and a sandbox/isolated worker if arbitrary compiled or Python code is allowed);
- request lifecycle (authenticate, authorize, resolve tenant and installed plugin revision, execute validated workflow, commit changes atomically or via outbox, audit, return typed result);
- failure behavior (timeouts, denied capabilities, invalid input, retries, idempotency, rollback);
- install/upgrade/uninstall/disable semantics with human approval for consequential updates;
- deterministic money/quantity rounding and immutable historical rule-version references.

The current CEngine v1 manifest schema does not yet include the proposed generic module/plugin import contract. **Do not claim the loader or ABI exists merely because these documents describe it.**

## MJCC initial manual plugin set

Map existing implementation and golden tests, without rewriting business behavior by guesswork:
- Inventory period with **three operational weeks**, opening, receipts, pulls and closing quantity.
- Opening cost carried from prior period; accepted incoming invoice unit prices; 25% price-change review and drift enforcement.
- Row-level money rounding, reconciliation, over-pull evidence and safe published-period freeze/reopen.
- Invoice upload/extraction/review, vendor/SKU catalog, manager approvals and change history.
- Menu cycles, HACCP logs, daily checklists, events, role permissions, printable reports and downstream UI behavior.

Prefer parameterized policies for simple variations and tenant plugins only for business-specific algorithms. **Manually author these now**, but use the exact contracts future Persona-generated plugins will consume.

## Implementation order for the next weeks

1. **Snapshot and audit (no production mutation):** verify deployed MJCC version, existing Compute frontend/FastAPI dependencies, DB/RLS/RPC and records, CEngine engine/server tests, current versions and any open PR overlap; create golden fixtures from permitted sanitized examples.
2. **Plugin vertical slice:** define host contract and get a small real inventory action running `MJCC UI -> CEngine HTTP API -> shared module -> manually created MJCC plugin -> test database` end to end. Demonstrate a second different policy plugged into the same host without CEngine source edits.
3. **Independent repo extraction:** after explicit authorization of target MJCC GitHub repo, copy/extract customer-specific UI, plugin and tests with clear KpnSolute-owned shared component boundaries. Keep the existing Compute-served application working during transition.
4. **Workflow migration:** replace relevant MJCC FastAPI business endpoints with CEngine API calls, preserving auth/session translation, data-isolation checks, invoice pricing, approvals and file operations. Do not leave duplicate writers.
5. **Template #1 evidence:** document generalized skeleton without private data; verify it can instantiate for synthetic tenant with different private policy.
6. **Release gate:** reconcile calculations and records with original system, test failure/rollback, verify tenant and secret isolation, security review, performance, backup restore, customer acceptance, and explicit Loom/release authorization. Cut over only after these pass.

## Definition of done

- MJCC has a separately maintained **authorized** source repository, with a stable UI and complete plugin source/tests, not a renamed fork indefinitely depending on MJCC-only Compute endpoints.
- The actual MJCC business execution path runs through **CEngine's API** and plugin workflow, not old application business logic.
- At least one module works against two distinct plugin implementations with no hardcoded customer branch in core.
- Exact MJCC business rules pass golden tests, including rounding, 25% review, invoices, role scopes, period locks and history.
- A failed plugin, GitHub outage or blocked update does not corrupt MJCC data or stop an already deployed approved plugin build from being loaded.
- Reusable skeleton is separate from private MJCC source and actual data; private customer material does not enter Templates V1.
- Evidence covers tested end-to-end functionality, not merely source code or documentation. No production claims until independently verified.

## Important sequence correction

Previous discussion suggested moving MJCC first and integrating CEngine later or keeping MJCC's own FastAPI API as a final backend. **The owner rejected that as the final target.** The intended work is **both concurrently**: build plugin execution and extract MJCC, with **CEngine API as the final backend**. Existing APIs may remain only as a reversible migration safety bridge.

Do not begin construction of Persona, full Platform, Infajn or customer GitHub App as prerequisites to this proof. Those remain future ecosystem work. Prioritize MJCC live and the engine/plugin proof.

## Required decisions and permissions before irreversible work

- Customer-authorized destination repo and source-license/ownership boundaries.
- First plugin execution format and isolation model.
- How the current auth mechanism and DB access are mediated through CEngine.
- Who can approve plugin releases and the MJCC production cutover.
- Approved Loom verdict on shared plugin ABI and any cross-product architecture conflicts.

**For Claude:** Read the previous three PR documents and this latest owner correction; compare with real code; supply a branch-based implementation plan with tests, dependencies, risk, and explicit blockers. No unapproved repo migration, credential use, schema deletion or live deployment.
