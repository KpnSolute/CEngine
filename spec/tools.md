# Built-in tools (DRAFT)

A tool is a domain-neutral JSON fragment (entities, state models, actions) that ships with
CEngine and that a template opts into by name. Tools are not templates (ADR-0008): they hold
no roles, prices or trade vocabulary, and the template's own `roles` decide who may use them.

Format (`crates/ce-tools/tools/<name>.json`): `tool`, `version`, `title`, `entities`, and
optionally `actions`. Composition (`ce_tools::compose(template, &["tasks"])`) merges the
fragment into the manifest before validation. It never overrides: a clash with an entity or
action name in the template is an error.

Rules: personal data is marked `pii`; a tool records intent only and never names a provider or
holds credentials (delivery and provider calls are Infajn's); state codes follow the state
registry classes.

First wave in 0.1, status:
- `customers`: `customer`, `customer_contact` (built).
- `tasks`: `task` with open, in_progress, done, cancelled and start/complete/cancel actions (built).
- `notifications`: `notification` records with pending, sent, read, failed; delivery is Infajn's (built).
- calendar and booking: logic in `ce-calendar` (built); its booking entity fragment is still to do.
- barcodes: logic in `ce-barcode` (built).

Second wave (invoicing, search, import/export) and the rest are not started.
