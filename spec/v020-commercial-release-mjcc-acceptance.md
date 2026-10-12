# CEngine v0.2.0 Release, MJCC Cutover, and Proprietary Distribution Decision

**Status:** Owner-directed product intent, proposed implementation and release gates. Document only; not a release, customer cutover, or claim of production readiness.
**Date:** 2026-10-12
**Owner:** KpnSolute / KpnLLC
**Read with:** `spec/plugins.md`, `spec/modular-runtime-mjcc-integration.md`, `spec/persona-private-plugins-pipeline.md`, `spec/customer-github-plugin-lifecycle.md`, `spec/mjcc-extraction-cengine-api-cutover.md`, `spec/customer-plugin-package-convention.md`. Loom ADR-0007/0008 and any later approved amendment take precedence on authorization and rollout.

## Product/ownership decision: proprietary, not open source

CEngine is intended to be **KpnSolute-owned, proprietary commercial software**, not an open-source project. Its source code, Rust crates, server code, plugin host, and internal execution interfaces must not be treated as freely redistributable assets.

**Observed current discrepancy:** `KpnSolute/CEngine` is currently a **public GitHub repository**. Root `Cargo.toml` has `license = "UNLICENSED"` and `publish = false`. Neither setting makes a public repository private, nor automatically grants an open-source license. However the complete public Git history may already have been accessed or copied.

**Required immediate owner/admin action:** decide and perform GitHub repository visibility change to **private** in GitHub repository Settings, subject to GitHub's warnings, access/deployment implications and existing clone/fork exposure. Audit contributors, credentials/secrets in history, package registries, external actions, release artifacts, mirrors and CI dependencies. If secrets were accidentally published, rotate them. This PR **does not perform the visibility change**, cannot retroactively revoke existing copies, and is not evidence it happened.

Choose a clear proprietary notice and written customer/internal license terms after legal review. Preserve attribution and third-party dependency licenses. Do not promise customers ownership of the shared engine; define separately the customer's ownership and access to customer-specific plugin source and their data. Avoid shipping private Rust source to customer GitHub repositories.

## Architecture: both library AND API

**One implementation of execution semantics; two consumption surfaces.**

- **Reusable Rust library/crates:** internal core, policy/expression evaluation, validation, Postgres data plane, shared modules, plugin loading/composition, safety/audit logic; versioned and consumed by authorized KpnSolute build pipelines.
- **CEngine HTTP API/server:** hosted execution boundary for customer applications, including MJCC UI. It owns business workflows, tenant/installation authorization, record APIs, state/actions, plugin execution, audit, idempotency and transactions.
- **Optional client SDKs:** thin public-contract clients for TypeScript/Python, not distribution of private engine internals and not alternate business-rule implementations.
- **MJCC own repository:** independently maintained UI/configuration/tests, and private MJCC plugin source under `.cengine/plugins/<slug>/plugin.toml`; never copy CEngine itself or use old MJCC FastAPI business routes as permanent executors.
- **MJCC tenant data:** separate authorized data environment. Shared KpnSolute template skeleton remains free of MJCC's confidential SOPs and records.

Flow: `MJCC UI -> deployed CEngine HTTP API -> registered module -> approved MJCC policy plugin -> isolated MJCC data + audit`.

Plugin-source repositories and the engine repository have different permissions and owners. GitHub is not a runtime database; installations use immutable approved plugin hashes and builds.

## Evidence known at time of drafting (do not exaggerate)

- CEngine has an existing inventory `business.inventory` module, policy slots, `ce-plugin` descriptor and digest-pinned loader, compositional manifest behavior, PostgreSQL HTTP tests under two different plugins, provenance/audit, and period rollover tests described in `spec/plugins.md`. Source/tests are promising, not production deployment proof.
- Claude reports MJCC-workspace PR #1 has an `mjcc-inventory` plugin and synthetic fixture, three computed rows matching hand-calculated cent values, and a refused over-pull (422) in local Postgres.
- **Critical limitation of that reported MJCC slice:** the MJCC plugin overrides **no policy slot**, because the custom behavior currently equals the inventory module defaults. Hence the MJCC-specific external plugin-to-CEngine execution path has **not been proved by that test alone**.
- The MJCC-workspace PR #1 could not be inspected through the currently connected GitHub account; its CI/test claims are therefore reporter-provided, not independently verified in this document.
- CEngine root `Cargo.toml` currently says `0.1.0`; the release workflow's input must match that value; the latest release observed was prerelease `v0.1.0`.
- `.github/workflows/release.yaml` manually accepts `version` and `prerelease`, verifies Cargo version, then creates a GitHub release/tag. It does **not**, by itself, run the complete v0.2.0 MJCC integration acceptance gate; ensure verified CI and additional release-gate tests before invoking it.
- A live public CEngine API, MJCC production use of that API, real-data parity, private-repository conversion and complete customer migration remain **unverified**.

## v0.2.0 release distinction

**Three separate verdicts:**

1. **Library/API pre-release is ready:** correct version/tag, green Rust+Postgres+Docker+conformance checks, tested plugin contract, immutable artifact, reviewed change log, compatible HTTP API. This permits controlled pinning by dependent test repositories.
2. **MJCC plugin integration is ready:** automated `MJCC-workspace` tests install a pinned engine version and a distinct plugin version, demonstrate an actual custom rule override or a deliberately different test plugin under the same hook, prove the engine executes it over HTTP (not legacy routes), then reconcile the real MJCC policy behavior with golden test cases.
3. **MJCC production is ready:** complete working UI calling a deployed/secured CEngine API; all operational endpoints migrated or explicitly feature-gated; production auth, data isolation, inventory, invoices, menus, logs, files, audit, backups, recovery, monitoring, performance, customer acceptance, and separate approved cutover. A version tag is not a production release verdict.

Owner preference: v0.2.0 should aim at **all MJCC-live gates**, not be advertised as complete after three synthetic inventory lines. It may be tagged as a clearly marked **pre-release** to unblock pinned integration CI before MJCC launch, provided the distinction is explicit and approved.

## Blocking v0.2.0 engineering work

**A. Version and release hygiene**
- Verify all crate/workspace versions, lockfiles, build metadata, workflow input, tag history and API compatibility; consistently bump to `0.2.0` if approved.
- Run all relevant automated tests: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets`, `cargo test --workspace`, DB-backed integration, conformance, Python/WASM bindings if in release scope, Docker install/serve/health.
- Pin an exact commit and release artifact, document changes/breaking API details, CI outcome and rollback. Don't generate the tag until version matches and checks pass.

**B. Prove independent plugin behavior**
- Load from approved `.cengine/plugins/.../plugin.toml` bytes tied to digest, registered hooks, module version, and a trusted installation binding.
- Run HTTP/Postgres tests with default rules and a different private plugin. Demonstrate different output/effect without a code change to CEngine.
- For MJCC, pin its own plugin and prove the request traverses the installed plugin. If all MJCC policies equal defaults, use a separate test-only override to establish the general mechanism; additionally verify MJCC-specific manifest/provenance and golden results. **Do not corrupt actual MJCC business rules just to create a differing outcome.**
- Make plugin installation records durable, composed manifest loadable by the real `ce-serve` production binary, and enforce approvals, scopes, version, rollback, and audit consistently.

**C. Automate MJCC CI**
- In the authorized MJCC repo, pin an immutable v0.2.0 CEngine dependency or container digest; run the existing synthetic fixture with actual CEngine HTTP API and PostgreSQL.
- Add regression cases: three operational weeks; receipts/pulls; over-pulls; opening cost carryover; accepted invoice price and the 25% review queue; price drift; period close/reopen; rounding to cents and reconciliation; role permissions; retried requests; failed plugin and rollback.
- Preserve private MJCC tenant material. Use synthetic CI fixtures and dedicated non-production DB.

**D. Prove a live CEngine API**
- Document and verify URL, TLS, auth/token verifier, health/readiness, database migration/install procedure, secret management, observability, safe shutdown/restart, backups and restore.
- Require end-to-end UI calls for MJCC business operations. The old MJCC FastAPI backend must not remain the actual decision engine behind a proxy while v0.2.0 is called plugin-backed.

**E. MJCC cutover**
- Keep existing production service and data untouched until explicit cutover approval, real parity evidence, tested rollback and customer sign-off. Maintain a migration plan separating repository extraction, backend execution replacement and live traffic switch.
- Check Loom ADR-0008's existing conditional gate and obtain a recorded exception/amendment if current implementation sequencing differs. Do not infer the CEngine library build authorization allows MJCC production changes.

## Commercial distribution and security

- Make engine source private through separately approved admin action; do not confuse hiding source with securing the HTTP API. Enforce per-tenant data authorization and secure plugin execution independently.
- CEngine can be installed as an internal pinned Rust dependency or container image in a private registry. Customer app repo uses API/SDK contracts and its own private plugins, **not source access to the shared engine**.
- Review whether engine binaries/SDKs need any customer redistribution rights. Define terms/contracts and keep signing keys, tokens and infrastructure secrets out of Git.
- Preserve auditability and private plugin source/license distinctions, IP provenance and third-party license compliance.

## Claude implementation deliverables before release request

1. Evidence matrix: existing vs missing vs tested vs deployed for each block above, with commits, exact test commands/logs, CI URLs and blockers.
2. CEngine v0.2.0 release candidate PR: version bump, contract tests, functional plugin hosting, actual server load/install path, and release notes.
3. MJCC repo integration PR: automated golden tests pinned to CEngine RC, real plugin hook proof, API calls, and complete feature-parity matrix.
4. Deployment/cutover proposal with non-production API test URL, approved credentials/owners, backups/restore, rollback plan and separately recorded Loom/customer release verdict.
5. Security/IP action list: private visibility conversion owner, credential audit, license review and authorized artifact distribution strategy.

**Do not publish the v0.2.0 tag, change repository visibility, access MJCC production data, deploy live CEngine, modify customer GitHub permissions, or cut traffic solely because this documentation PR is merged.**
