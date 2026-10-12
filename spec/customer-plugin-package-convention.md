# CEngine Customer Plugin Package Convention (proposed v1)

**Status:** Proposed packaging contract and implementation target. **Not yet implemented by CEngine.**
**Date:** 2026-10-12
**Relationship:** Companion to [MJCC extraction and CEngine API cutover](./mjcc-extraction-cengine-api-cutover.md), plus earlier PRs #48, #50 and #52. This document proposes how a customer's GitHub repository exposes plugins to CEngine without copying customer-specific logic into the engine.

## In everyday terms

Like `.github/workflows/` tells GitHub where a workflow is described, **`.cengine/` tells CEngine where the workspace's integration and plugin declarations live**. Each plugin has its own folder with a `plugin.toml` file. The file is a recipe card; it says what the plugin is, which module/workflow it extends, how it is built/tested and what permissions it asks for. It is *not* permission to execute arbitrary code.

## Standard structure of a customer-owned GitHub repository

```text
mjcc-workspace/
├── .cengine/
│   ├── workspace.toml               # repo/workspace descriptor, no secrets
│   ├── plugins.lock                 # immutable approved versions/digests (generated)
│   └── plugins/
│       └── inventory-rules/
│           ├── plugin.toml          # required plugin manifest
│           ├── src/                 # rule source or plugin implementation
│           │   └── rules.py        # illustrative; language not standardized yet
│           ├── tests/
│           │   └── test_rules.py
│           └── fixtures/
│               └── inventory.json  # synthetic data only
├── app/                              # MJCC-owned UI and assets
├── tests/                            # end-to-end app / API tests
└── README.md
```

Plugin discovery **only** scans explicitly installed folders under `.cengine/plugins/<slug>/plugin.toml` from an approved commit. It does not recursively execute arbitrary source files; GitHub Actions files do not automatically grant runtime permissions. A single repo may contain multiple plugins.

## Example: `.cengine/workspace.toml`

```toml
schema_version = 1

[workspace]
name = "mjcc"
template = "foodservice.operations"
template_version = "0.1.0"

[plugins]
directory = ".cengine/plugins"
lockfile = ".cengine/plugins.lock"
```

These example identifiers are placeholders. A manifest must not trust a customer-written tenant ID as evidence of access. The platform/CEngine installation resolves the authorized tenant and workspace using trusted identity and installation metadata, not strings in Git.

## Example: `.cengine/plugins/inventory-rules/plugin.toml`

```toml
schema_version = 1

[plugin]
id = "inventory-rules"
version = "0.1.0"
description = "MJCC-specific inventory calculation and approval policies"

[compatibility]
engine = ">=0.2.0, <2.0.0"            # illustrative constraint; not verified engine release support
module = "business.inventory"
module_api = "1"

[execution]
kind = "isolated-python"               # proposed host type; NOT shipped
entrypoint = "src/rules.py:InventoryRules"
timeout_ms = 2000
memory_mb = 128
deterministic = true

[capabilities]
read = ["inventory.items", "inventory.periods", "inventory.receipts"]
write = ["inventory.periods"]
emit = ["inventory.review_requested"]
external_network = false

[[extensions]]
hook = "inventory.calculate_closing"
input_schema = "schemas/closing-input.json"
output_schema = "schemas/closing-output.json"

[[extensions]]
hook = "inventory.review_invoice_price"
input_schema = "schemas/review-input.json"
output_schema = "schemas/review-output.json"

[tests]
command = "python -m pytest -q tests"
fixtures = "fixtures"
```

This is **illustrative TOML**, not a valid current engine manifest or working plugin loader configuration. The real host types, hook registry, capabilities and schemas must be defined and verified before code generation. The engine should reject unknown or unsupported plugin formats, not attempt an unsafe fallback. `plugin.toml` contains no GitHub token, credentials, production data, private SOP files or secret environment values.

## Required fields and validation

Proposed required values: `schema_version`, `plugin.id`, `plugin.version`, engine/module API compatibility, execution kind, allowed hook(s), requested capabilities, typed input/output schema locations and tests. All paths must be relative, normalized and inside the plugin root. Deny absolute paths, `..` traversal, symlink escapes and executable artifacts not included in the pinned build digest. Deny duplicate IDs and unknown hooks.

Plugin IDs: lowercase ASCII slug, numerals and hyphens; stable within the authorized workspace. Version: semantic version. Fail closed for incompatibility. Module registry, hook schema and `capabilities` vocabulary are controlled by CEngine, **not invented by each plugin**.

The build system resolves `plugin.toml` from a specific Git commit SHA; validates its schema, version compatibility, dependency lock and requested capabilities; checks source and fixture contents; runs deterministic tests and isolated execution checks; produces a signed immutable artifact. **Approved** plugin version and build digest are recorded in `plugins.lock` and the trusted workspace installation database; a customer-editable lockfile alone must never be accepted as proof of approval.

## Proposed plugin loader lifecycle

1. **Connect:** Customer authorizes KpnSolute's GitHub App for a selected repo. For MJCC bootstrap, obtain explicit approval of the target repository.
2. **Discover:** An authorized update/build job fetches a pinned commit and scans only registered `.cengine/plugins/*/plugin.toml` paths.
3. **Validate:** Check TOML schema, declared module/hook compatibility, path integrity, requested scopes, dependency lock and code provenance.
4. **Build/test:** Compile/package in isolation, run the plugin's tests plus CEngine host conformance vectors, secret/security checks and golden MJCC financial/inventory cases.
5. **Approve:** Customer-authorized reviewer accepts the exact commit, plugin hash, scopes and resulting artifact digest. Financial/external writes get stronger review.
6. **Install:** CEngine installation pins the approved artifact and hook bindings. The engine loads only approved artifacts through a sandbox/isolated worker or declared safe rule evaluator.
7. **Execute:** MJCC UI calls CEngine's API. CEngine authorizes the request and tenant, uses a shared module workflow, invokes the pinned MJCC plugin and durably records its results and audit.
8. **Update:** Persona (later) or a maintainer edits private plugin source **only upon an authorized customer request**, submits it for tests/review and activates a new immutable version. Old builds remain available for rollback.
9. **Disconnect/disable:** Revoking GitHub access prevents future code fetch/update; runtime continuation of a previously approved artifact depends on an explicit customer contract and policy. Disable unsafe plugins without erasing audit history.

Daily CEngine execution **does not fetch latest GitHub HEAD or invoke Persona/Claude to run calculations**. An outage at GitHub must not stop an already approved installed plugin.

## Runtime rule: execution authority remains in CEngine

The customer repo owns the UI and plugin source, not a parallel MJCC business API. All authoritative business operations traverse:

```text
MJCC UI -> CEngine HTTP API -> shared module/workflow
        -> approved MJCC plugin -> validated transaction + audit -> MJCC data
```

CEngine decides which plugin can be invoked, the accessible data, transaction boundaries, role permissions, timeouts, and side effects. Plugin code supplies *business-specific behavior*, not uncontrolled database connections or secrets. Provider activity belongs behind Infajn's eventual adapter boundary or explicitly documented temporary connector.

## Future Persona generation compatibility

Persona should eventually generate exactly the same `.cengine/plugins/<slug>/` package, tests and manifest that developers create manually for MJCC now. There is **one stable plugin contract**, not a hand-coded MJCC exception and a separate AI-generated format. Persona is invoked for creation or requested update; no LLM call is required for normal business transactions.

## Release acceptance tests for this convention

- A valid plugin package is discovered from an authorized repo/commit; malformed TOML, path traversal, unsupported host and missing schemas are rejected.
- A plugin can implement a named hook, pass conformance and run through the CEngine HTTP workflow; the runtime does not execute unregistered functions.
- Two tenant installations cannot load or read each other's private plugin/data, even with identical plugin IDs.
- A different plugin for the same hook can be substituted with **no CEngine core code change**.
- Wrong major module API version, excessive capabilities, timeout, exception, invalid output and denied network requests are rejected safely.
- Active builds survive GitHub outage and source branch changes; version rollback is tested.
- MJCC quantity, valuation, price-review and close outputs match approved golden fixtures.
- No customer plugin code or private SOP/data is published in global Templates V1.

## Remaining implementation decisions

- Do we implement only declarative rules first, then `isolated-python`, or implement both for launch? The example must not be used as a commitment to support arbitrary Python.
- What is the first **real** inventory module hook interface, return schema and error model?
- How do transactions and side effects cross the plugin-host boundary?
- How are signed build artifacts stored and retrieved if Infajn is not deployed yet?
- What exact GitHub App/repository authorization, customer approvals and disconnect policy applies?
- Who controls plugin update approval and production release for the MJCC customer?

**Owner-approved direction:** Standardize the package layout, use customer-connected GitHub source, load private plugins through CEngine and keep Persona out of daily execution. **Engineering contract details above are proposed until Loom and maintainers approve them.**
