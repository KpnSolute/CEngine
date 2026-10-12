# CEngine Third-Party License Review

**Status: Not complete. No license-compliance clearance is claimed.**

The repository's `Cargo.lock` contains direct and transitive Rust package dependencies. The proprietary CEngine notice does not relicense third-party dependencies. Additional reviews are needed for Docker base images, build tools, copied snippets and other incorporated assets.

## Audit workflow required before commercial distribution

1. Run `cargo metadata --locked --format-version 1` and produce a pinned software bill of materials (SBOM) for the release commit.
2. Use a maintained dependency-license scanner (for example `cargo-deny` and `cargo-license`) to inventory **all** resolved package versions. Review ambiguous, unknown, dual-licensed, GPL-family, LGPL, AGPL, MPL and other reciprocal obligations individually. Do not assume a scan result is legal advice.
3. Inventory non-Cargo dependencies, copied source, fonts/media, binary/runtime distributions, Docker base images and third-party API SDK terms.
4. Review obligations for linking, source disclosure, preservation of notices, attribution, patent clauses, and distribution of container images and binaries; distinguish hosted API use from distribution.
5. Record each dependency, exact version, license expression, chosen license if dual-licensed, obligations, and reviewer disposition; generate a compliant NOTICE/third-party attribution bundle where required.
6. Run the audit in CI and block release artifacts with unresolved **material** license issues, subject to a documented reviewer verdict.
7. Audit human contributions and AI-assisted code provenance for ownership and potential third-party copying issues.

## Interim treatment

- Keep the workspace `publish = false` and `license = "UNLICENSED"` unless a reviewed licensing decision changes them.
- The top-level proprietary notice covers only original code whose rights are properly controlled. It does not invent copyright ownership for a username or unregistered brand.
- Do not copy licensed dependencies into the repo or distribute compiled products without reviewing their obligations.
- Keep v0.2.0 engineering running; the legal audit is a **distribution gate**, not permission to falsely claim the engine is release-ready.
