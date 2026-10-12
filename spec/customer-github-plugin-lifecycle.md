# Customer-Connected GitHub: Private Plugins and MJCC Template #1

**Status:** Owner clarification / architecture specification proposal for Loom review, dated 2026-10-12. This clarifies and supersedes conflicting *proposed* text in [modular-runtime-mjcc-integration.md](./modular-runtime-mjcc-integration.md) (PR #48) and [persona-private-plugins-pipeline.md](./persona-private-plugins-pipeline.md) (PR #50). It does not override accepted Loom ADRs or authorize production deployment.

## What the owner has clarified

1. **MJCC (Miami Job Corps Center) is the first independent customer tenant**, currently implemented within `KpnSolute/Compute`. MJCC is not a company subsidiary, not the CEngine product, and not itself a publicly reusable source of customer data.
2. The reusable *technical skeleton extracted from MJCC* will become **KpnSolute Templates V1 template #1**. Actual MJCC operations, private SOPs, records, credentials and company-specific plugin code stay outside the shared template registry.
3. **Modules** are reusable runtime business libraries and tools. **Plugins** are customer-specific extensions, policy sets and code that customize those modules. Plugins are installed into a tenant's workspace and behave as part of the running application. They are **durably saved but not published to the global reusable registry**.
4. **Every customer brings/connects their own GitHub account** and authorizes KpnSolute to work in their selected GitHub repositories. The customer should not be instructed to manually upload plugin files. KpnSolute creates/updates the files via authorized GitHub integration. Do not presume permission to create repositories in customers' accounts or use any repo without explicit authorization.
5. **Persona works on demand**, primarily when the customer requests creation or a change to the workspace. Persona is not part of routine plugin execution. The initial MJCC plugin set can be authored manually using the same interfaces Persona will eventually generate.
6. CEngine should load **approved and version-pinned plugin builds**, not blindly run arbitrary latest source from GitHub. GitHub is the customer-controlled source of truth for their plugin code; the workspace and runtime retain only the deployment references and privately scoped operational metadata needed to restart, verify and roll back.

## Corrected pipeline and control plane

```text
Customer, via KpnSolute Platform
  -> AI system / budgeted model gateway (Claude API intended)
  -> Persona: private profile of business requirements, SOPs, selected modules,
     customized rules and plugin source (only on explicit create/change requests)
  -> Templates V1: select or register generalized, reusable KpnSolute skeleton
  -> Authorized GitHub integration: write private tenant-specific plugin source
     into the customer's connected repository, ideally on a proposed branch/PR
  -> Build, validation, isolated tests, human/customer approval
  -> Immutable signed plugin artifact + commit/digest + configuration reference
  -> Infajn: provision resources / provider integrations when needed and approved
  -> CEngine: run template modules and the approved tenant-private plugin version
  -> KpnSolute Platform: ongoing workspace/resources view, settings and requests
```

The stages may interleave; the diagram assigns ownership, not a single synchronous request. Persona **does not** control production operations, and a model response cannot authorize its own deployment or external writes.

## GitHub integration contract (proposed)

**Onboarding:** Customer chooses `Connect GitHub` in Platform. Prefer a least-privilege GitHub App installation or equivalent per-repository authorization, with explicit selected repositories, permitted content operations and identity of the granting organization/user. Handle organization approval policies gracefully. Do not request a password or broad personal access token. Store installation/access credentials in a vault, never in the template, plugin, frontend storage or Git repository. Repositories may already exist; whether Platform is allowed to create one is an additional, separately permissioned decision.

**Repository selection and bootstrap:** In the connected account, customer authorizes a particular repository (or explicitly authorizes creation). KpnSolute writes/maintains plugin files, a descriptor, dependency/version lock, tests, and machine-readable provenance without customer data, tokens or raw confidential SOPs in commits. The user does not manually push plugin files. Avoid forcing a specific layout or language until the plugin host contract is approved.

**Edit/update request:** Persona reads only the selected approved paths/versions and relevant private SOP material through authorized services; proposes a diff in a dedicated branch. Before merge or activation, run schema checks, dependency and license checks, unit/golden tests, malicious code / secret checks, tenant permissions validation, sandbox resource tests and review. Record whether a person or policy authorized the change and the reasons. No unconditional automatic merge of executable changes.

**Deploy:** Resolve exact repository + commit SHA + path + plugin manifest digest. Build a reproducible artifact in isolation, sign/pin it, deploy using an allowed private artifact location or runner. CEngine loads that approved artifact and compatible module interfaces. Do not import live Python directly into a privileged Rust process or execute arbitrary GitHub HEAD at request time. For company plugins stored as declarative policy only, the same version pinning and validation principles apply.

**Runtime:** Plugins use host-provided typed operations, tenant-scoped inputs, no unrestricted provider access, no raw credentials, explicit side-effect authorization and deterministic money/stock calculations. Daily operations do not depend on GitHub uptime or call Claude. Infajn handles external calls and provisioning, under CEngine authorization.

**Revoke/change ownership:** Respect GitHub App uninstall/repo de-selection and tenant disablement. Stop reading/writing remote source and stop future updates; define whether previously approved private builds may continue running and for how long under contract. User can disconnect, export, transfer and revoke, with understandable effect and audit records. Handle rotated keys, suspended accounts, renames, deleted repositories, force-pushes, missing commits and inaccessible build dependencies.

**Privacy:** Private GitHub repository does not automatically make its contents contractually safe to copy. Source ownership/licensing belongs to the customer and contracts; KpnSolute cannot promote a customer's plugin into Templates V1 without proper rights and privacy review. Keep a distinct, KpnSolute-owned generalized template.

## Source code and deployed code are different objects

- **Customer GitHub repo**: private durable source and history; created/updated on request.
- **Private build artifact**: immutable tested deployable package, signed and identified by digest.
- **Workspace installation record**: tenant/workspace ID, selected template and module versions, source repo reference, approved commit and build digest, runtime config and rollback pointer.
- **Customer's data**: authorized databases / connected services, separate from source, template and plugin binaries.
- **Global Templates V1 registry**: reusable skeletons only.

GitHub is a source-control provider, **not a runtime dependency or a production database**. Fetching source “when ready” means at the build/update stage, not fetching arbitrary code on every user request.

## Decisions required before coding GitHub automation

1. Is the minimum supported GitHub authorization a customer-installed GitHub App with per-repository selection? Strongly recommended.
2. For v1, does KpnSolute write to an existing customer-approved repository only, or may the customer additionally authorize repo creation?
3. Do customers approve every executable plugin PR/change or only changes with financial/external side effects? Establish role/approval matrix.
4. Are plugins declarative, sandboxed WASM, isolated Python workers, or a staged combination? The current CEngine manifest lacks generic module imports/plugins; specify ABI and isolation before implementing.
5. Where do signed immutable builds live, and how are rebuild, rollback, retention, export and customer disconnect managed?
6. Is GitHub required for every customer, or can a customer choose another supported private code store later? Owner currently expects connected GitHub, so don't invent an alternative for v1 without approval.
7. How is the initial MJCC customer repo selected/authorized, and how is the existing `KpnSolute/Compute` code separated without disrupting live operations?

## MJCC migration and v1 launch constraints

- Extract the MJCC *skeleton*, not the live MJCC records, into Templates V1 as template #1.
- Keep the MJCC tenant and live service intact while modeling the first private plugins from its approved three-week inventory, receipt/issue accounting, invoice price review, cent rounding, period close, SOP actions, and other operations.
- For the first iteration, manually author plugin source in an approved *MJCC-connected* GitHub repository rather than copying code across into the shared template registry. Customer GitHub authorization and ownership must be established before any access or writes; this document authorizes none.
- Run CEngine in a parallel isolated environment and verify golden cases for calculations, approvals, roles, period locks, tenant isolation, data consistency, and rollback before changing production.
- The general Persona auto-generation pipeline, broad industry catalog, and full third-party plugin ecosystem can grow after the initial **CEngine + MJCC tenant + Template #1** execution slice passes. A weeks-long deadline is a planning objective, not a waiver of acceptance tests.

## Reading order and precedence for Claude

1. Read PR #48's [modular runtime proposal](./modular-runtime-mjcc-integration.md).
2. Read PR #50's [Persona/private plugins proposal](./persona-private-plugins-pipeline.md).
3. Read **this clarification**. On *GitHub source ownership*, *Persona on-demand lifecycle* and *MJCC first-tenant/template status*, use these later owner clarifications rather than the earlier proposal wording.
4. Read Loom's accepted ADR-0007 / ADR-0008 and product change-control documents; obtain a verdict for material changes. Accepted Loom decisions outrank this proposed document.
5. Audit current code, version tags and tests. Report missing contracts and integration work. Do not present an approved PR, working GitHub App, deployed plugin execution or production MJCC cutover as completed just because this document exists.

No production changes, API entitlement claims, purchases, or customer GitHub access are authorized by this document.
