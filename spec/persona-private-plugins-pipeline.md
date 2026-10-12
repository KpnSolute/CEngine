# Persona-to-CEngine Pipeline: Private Generated Plugins and Templates V1

**Status:** Proposed architecture addendum for KpnSolute owner and Loom governance review. Not a claim of shipped functionality, release authorization, or approval to deploy.
**Date:** 2026-10-12
**Companion:** [Modular runtime and MJCC first-tenant plan](./modular-runtime-mjcc-integration.md).
**Readers:** Claude, CEngine maintainers, Compute/Platform, Persona, AI system and future Infajn teams.

## The owner's intended system

KpnCompute is an ecosystem of layered workspace generation and operation, not just a template viewer or a collection of dashboards.

**Interactive platform:** The customer-facing KpnSolute Platform is the day-to-day place where the user chats with AI, uploads or authorizes SOPs, manages generated artifacts, configures connectors, reviews proposed changes, and monitors workspace resources. Product identity, commercial account, budgets and tenant boundaries must remain distinguishable.

**AI system:** A budgeted model gateway, with Claude API as the intended model provider, supplies reasoning and generation to Persona. Use model-agnostic internal contracts; no infrastructure component should rely on prompt text as an authorization credential.

**Persona:** Builds the customer's business profile *for this engagement* from authorized SOPs, roles, processes, goals and integration metadata; determines needed capabilities; creates custom rules and private plugins when pre-existing modules cannot express the behavior; outputs reviewable, versioned artifact and template proposals. The profile and raw customer inputs are private, not contributions to a shared public gallery.

**Templates V1:** KpnSolute's registry of reusable, platform-owned skeletons. A skeleton can declare entities, views, modules and workflows; it must never include another customer's records, confidential SOP text, private generated plugin implementation, credentials, secrets, or tenant-specific configuration. Customer-specific instances and bindings live outside the shared registry.

**Infajn:** The proposed infrastructure/resource and provider-interaction component. Infajn provisions and operates external resources and performs authorized provider calls. CEngine controls policy, entitlement, operations, auditing and results; Infajn does not decide a customer's inventory formula.

**CEngine:** Executes validated template instances and their allowed modules, selected policies and private plugins against tenant-scoped data. It must continue to work deterministically for ordinary runtime operations without an AI call on each calculation.

The names and exact inter-service APIs remain subject to Loom's existing ADR-0007 / ADR-0008 decisions. Product repo documentation cannot override those decisions.

## A concrete mental model

- **CEngine core** is analogous to a programming language runtime: common execution, safety, data and authorization semantics.
- **Modules** are analogous to importable libraries such as `discord.py`: reusable business capabilities, e.g. inventory, invoices, workflows, appointments, HR, reports.
- **Private plugins** are tenant-specific implementations of well-defined module extension points. MJCC's valuation and price-review policies are the initial concrete example. Persona should eventually generate these on demand; for MJCC launch, engineers can author them manually to the *same* interface.
- **Template** is the reusable skeleton: what capabilities an application needs and how they compose.
- **Workspace instance** is a customer's actual operating system, binding that skeleton to its own approved private rules and isolated records.

**Important:** A company-specific plugin is never automatically promoted into a shared module or Templates V1. Building one is not evidence of reuse rights. Reusable generic *ideas* may be incorporated separately after privacy, licensing, ownership and approval checks.

## Pipeline and approval boundaries

```text
Customer / staff
   |
   v
KpnSolute Platform (conversation, artifacts, workspace administration, billing)
   |
   +--> Budgeted AI gateway (Claude API intended; spend limits and usage audit)
   |         |
   |         v
   +------ Persona (private SOP analysis + business profile)
                |
                +--> Modules registry: find available capabilities
                +--> Templates V1: fetch KpnSolute-owned skeleton(s)
                +--> Private rules / plugin generator
                |         |
                |         v
                |     Validation + sandbox testing + human approval
                |
                v
       Workspace blueprint / template-instance binding
                |
                +--> CEngine validates manifest, resolves imports and policy bindings
                +--> Infajn provisions/connects approved external resources
                |
                v
       Customer's running workspace
       (CEngine execution + private plugin + isolated business data)
```

This drawing shows logical responsibility, not a decision to put AI processing physically inside the Platform or to deploy Infajn before it is available. Provisioning and runtime initialization can be asynchronous, retried and resumed; no single HTTP request should own the full sequence.

## Private plugins are not global saved plugins

The owner has specified that Persona-created plugins are **generated for the company and not shared or saved in KpnSolute's reusable global registry**.

Operational clarification (proposal): an *active* plugin must still have a durable, private, tenant-scoped representation or a deterministic, pinned deployable artifact. This is necessary for restart, audit, rollback, investigation and replay of calculations. Otherwise a service restart could regenerate incompatible behavior or strand business records.

Two storage strategies remain open for owner choice:
1. Tenant-private source and a signed build artifact, accessible only to explicitly authorized operators and the tenant's control plane.
2. Signed, tenant-private compiled artifact with configuration and pinned version/hash in the workspace, while retaining the minimum source/escrow material contractually required for maintenance and recovery.

In **either** strategy: neither plugin source nor binary is listed as a global reusable template. Do not put private plugins in shared platform indexes, other tenants' execution environments, prompts for other businesses, or model-training corpora without explicit rights. Never embed secrets in plugin code or manifest.

## Proposed interface contracts (NOT YET SUPPORTED BY CURRENT CEngine SCHEMA)

### Reusable module descriptor

A module would declare:
- stable `module_id`, semantic version, engine compatibility and dependencies;
- named capabilities (operations, entities, actions, read/write scopes and events);
- typed extension points, including allowed contexts, error types and side effects;
- safe default rule implementations and conformance tests;
- resource requirements, supported execution model and compatibility/deprecation policy.

### Private policy/plugin descriptor

A plugin would declare:
- `tenant_scope`, `workspace_id`, `plugin_id`, immutable version and content digest;
- target `module_id`, extension point, host/API version, input/output schema;
- capabilities: what it may read, write, emit, or request from Infajn;
- memory/CPU/time limits, deterministic clock/randomness contracts if needed;
- lifecycle: proposed → validated → tested → approved → active → superseded/disabled;
- source provenance, Persona run metadata (without leaking private SOP contents), tests, human approval and rollback target;
- financial effective date and calculation-rule version if outputs affect valued records.

No template may contain executable plugin code, broad database tokens or credentials. Templates can describe module requirements and extension-point *types*; only a private workspace binding chooses a company-specific implementation.

### Invocation contract

CEngine's module invokes a selected policy with validated, minimal tenant-scoped inputs; the policy returns a typed deterministic result or explicit error. Core policy checks enforce whether the operation can proceed. External mutations are never accidental function side effects: the host validates/approves them and routes them through Infajn with idempotency and audit.

Example illustrative interface:

```text
evaluate_policy(
  tenant_scope,
  module="inventory",
  extension="valuation",
  plugin_version="mjcc-valuation@1",
  inputs={period, opening_cost, receipts, issues, approved_prices}
) -> {quantity, amount, warnings, provenance}
```

This is **pseudo-contract**, not an available API. Do not interpret it as permitting plugins to query arbitrary tenants or to modify external providers directly.

### Generated rule tiers

1. **Configuration** for parameters (operational weeks, thresholds, approval routes).
2. **Declarative rules** for expressions, invariants, state transitions and arithmetic that the safe language can represent.
3. **Private plugin** for complex specialized logic needing an explicitly defined host extension point.

Persona should always attempt (1) or (2) first. Generated code is the last resort, not the default, because it increases validation, sandbox and maintenance costs.

## Security and correctness of AI-generated plugins

Treat every uploaded SOP and model output as **untrusted**, even if supplied by an authenticated customer. It may describe business requirements but cannot authorize spending, broad credentials, data exfiltration, cross-tenant access, provider writes or production releases.

Before activation:
- Generate a schema-valid proposal and dependency lock, never direct privileged code execution from an LLM response.
- Run static checks, dependency/license screening, sandbox tests, resource-boundary tests and module contract conformance.
- Verify the outputs against customer-approved examples, particularly negative cases, rounding, time-zone changes, high-value operations and concurrent updates.
- Review capabilities and side effects separately from calculation correctness.
- Require the appropriate human approval for production-impacting behavior.
- Pin plugin version to affected records and preserve immutable provenance; updates require migration and rollback procedures.
- Sandboxed WASM might be a future mechanism. It is **not** a substitute for designing a host interface and is **not** claimed to exist today.

Financial, payroll, healthcare, and banking workflows require specialized regulatory and security review before exposure. Plugin-generated numbers must not silently override the authoritative historical ledger.

## AI budgeting and operational cost boundaries

The AI system should enforce limits per user, workspace, task and billing tier:
- allowed models and maximum context, token output and tool invocations;
- budget reservations before generation, attributable spend after provider usage, retries and cancellation;
- approval thresholds for over-budget runs;
- cost attribution by Persona generation, testing, regeneration and maintenance, not merely by chat message;
- quotas and rate limits for sandbox builds and expensive connector operations;
- monitoring and anomaly alerts to avoid one workspace exhausting the whole system.

**Runtime principle:** normal inventory reads, period calculations and other approved deterministic plugin invocations do not call Claude each time. AI can help *design* the behavior; CEngine reliably *runs* it. This protects latency, repeatability and gross margin.

## MJCC immediate execution plan for upcoming v1

The owner states development is currently around v0.2 and is preparing v1 within the coming weeks. Verify tags, builds, tested API surfaces and CI before reporting an authoritative version. The repository root `Cargo.toml` had still shown `0.1.0` on review; do not silently alter release metadata without a versioning decision.

**Minimum MJCC-compatible slice:**
1. Identify the approved, reusable inventory / states / changesets / periods capabilities already available in CEngine and the exact missing module extension interfaces.
2. Hand-author MJCC's private valuation, three-week inventory, invoice price-review and rollover rules via declarative policy where possible and a private plugin only for genuinely unsupported behavior. These same interfaces become Persona's later generation targets.
3. Keep MJCC records and SOPs out of Templates V1. Create a generalized food-service skeleton plus an MJCC-private binding/configuration.
4. Validate against golden fixtures from the live Compute rules: opening + receipts − pulled, floor-to-zero display, over-pull alerts, opening-value carryover, latest approved invoice price, 25% price-review threshold, cent rounding and period locks.
5. Integrate existing Compute UI and authentication through an explicitly scoped adapter; preserve production as source of truth during a parallel validation run.
6. Demonstrate separate synthetic tenant isolation and reproducible installation of the same skeleton with a different private policy.
7. Only consider production migration when the tenant owner accepts reconciled results, backups/rollback exist, no data is lost, and an independently authorized release verdict exists.

Infajn may use temporary narrow adapters if its full implementation is not ready; those are documented technical debt, not proof a general connector/provisioning system has shipped.

**Do not block the entire MJCC v1 on fully automated Persona plugin generation, a global plugin marketplace, or all 15 industry groups.** The v1 launch should prove the module-extension contract and safe company-private policy execution. Persona automation can target that proven contract later.

## Outstanding decisions for Loom and the owner

1. Tenant-private plugin format: declarative only for v1, or signed arbitrary-code execution in a true sandbox?
2. Durable tenant-private plugin representation and its ownership/export/retention terms.
3. Who can approve new or updated company-specific plugins, and which actions need two-party approval?
4. Concrete module descriptor/import-resolution format and explicit CEngine manifest schema version.
5. Infajn's first deployable interfaces and provider adapters; what can launch with compatibility adapters?
6. Persona's private profile retention and opt-in/opt-out rules.
7. Token and sandbox-compute budget policy by plan and tenant.
8. MJCC acceptance test owner, evidence, supported rollback and release authority.

## Instructions for Claude and implementing teams

Read the earlier modular-runtime document **first**, then this addendum, then actual CEngine code and the approved Loom decisions. This is a **specification proposal**, not an instruction to change production or a claim that Persona/plugin runtimes already exist. Classify each required capability: implemented and tested / implemented not verified / missing / deferred. Return the smallest safe patch set, compatibility issues, tests, open approvals and deployment plan before changing the core or MJCC. Preserve the user-stated deadline urgency without skipping security, reproducibility or permission gates.
