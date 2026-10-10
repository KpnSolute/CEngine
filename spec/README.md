# CEngine Phase 1 spec (DRAFT)

Status: draft under Loom ADR-0007 (Approved with conditions). Kept outside every
product repository on purpose. Nothing here is committed, published or
implemented. Move into KpnSolute/CEngine only after ADR-0007 conditions 1 to 5.

## Contents
- `manifest.schema.json`: JSON Schema (2020-12) for a template manifest.
- `catalogs/errors.json`, `events.json`, `state-registry.json`: error, event and state-code catalogs.
- `expression-language.md`: full expression language (grammar, types, null rules, functions, where allowed).
- `languages.md`: how Rust, Python and JS/TS are used to build and consume the framework.
- `reference/expr_ref.py` and `conformance/expression-vectors.json`: tiny reference evaluator and 20 vectors; all pass.
- `examples/inventory.json`, `booking_crm.json`, `approval_workflow.json`: three unlike templates.

## Check performed
All three examples validate against the schema, and their state graphs have
unique codes and only point to declared states (script run 2026-10-10).

## Not yet specified (found while drafting)
1. **Expression language.** Now drafted in `expression-language.md`. Still open: aggregates over `children()` and SQL compilation are specified but have no vectors yet; the three example manifests are not yet type-checked against the language.
2. **OpenAPI generation.** Not drafted; follows the manifest and the data-plane endpoints in the contract plan.
3. **Entity state vs enum.** Examples declare both an enum field and a `state` model for the same field. Decide whether `state` replaces the enum.
4. **Role grammar.** `read`/`write` use entity names and `*`; field-level rules are undefined.
5. **Migrations.** Manifest-to-schema diff and the `migrate` plan/apply format are not specified.
6. **Breaking-change rules** for manifest versions are not written.
