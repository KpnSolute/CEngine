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
- `invoicing`: `invoice` (draft, sent, overdue, paid, void) and `invoice_line`, with send/overdue/paid/void actions (built). Totals are derived, never typed in: a line's `line_total` is `round(quantity * unit_price, 2)`, the invoice's `subtotal` is the sum of its lines' totals, and `total` is `subtotal + tax`. The server updates the invoice in the same transaction as any line write. Currency code, non-negative amounts and due date are invariants. No payment collection: that is a payments connector, made through Infajn.
- `bookings`: `resource` and `booking` (requested, confirmed, cancelled, completed, no_show) with confirm/cancel/complete/no-show actions (built). The scheduling logic (recurrence, conflicts, free slots) is `ce-calendar`; wiring it to the server endpoints is still to do.
- barcodes: logic in `ce-barcode` (built).

Invariants (such as a booking's end after its start) are enforced by the server on create and patch (`ce_validate::check_invariants`). Computed fields are evaluated first (`ce_validate::evaluate_computed`: dependency order, decimals rounded half-up to the field's scale), so rules can read them. Rules that use `ref()` or read a computed sum over other records are skipped for now; `unenforced_invariants` lists them.

Second wave: invoicing is built; `ce-io` does CSV import and export (see its crate docs). Search and the rest are not started.
