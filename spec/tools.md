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
- `audit`: `audit_event`, one row per create, update or delete: entity, record id, operation, actor, request id, time (unix seconds), versions, and the record before and after. The server writes it in the same transaction as the change, so a write that fails leaves no row and a replayed create adds none. Nobody can write it through the API (403); roles that can read it see the trail. The audit entity itself is not audited.
- `changesets`: `changeset` and `changeset_op`. An entity marked `"governed": true` takes writes straight from callers whose role has the `merge_changeset` action; every other caller's create, update or delete is staged and answers 202 with a changeset id. `X-CE-Changeset` adds to the caller's own open changeset. `POST /v1/{inst}/changeset/{id}/{merge|reject|revert|withdraw}`: merge, reject and revert need `merge_changeset`; only the submitter withdraws. A merge applies every operation in one transaction with audit, or none; if a record changed since staging the changeset is marked `conflicted`. Revert undoes a merged changeset in reverse order; one that deletes records cannot be reverted (422). The two entities are written by the server only (403).
- `period`: `period` (label, start and end date, `status` open or closed, who closed it and when, a snapshot). `locked_when` is now enforced on every write: an entity that says `ref(period).status == 'closed'` cannot be created in, edited in, moved into or deleted from a closed period (409 `entity_locked`). `POST /v1/{inst}/period/{id}/{close|reopen}` needs the `close_period` action; a close stores how many live records of each entity point at the period. Only these endpoints set the status fields (403 otherwise); both leave audit rows.
- `bookings`: `resource` and `booking` (requested, confirmed, cancelled, completed, no_show) with confirm/cancel/complete/no-show actions (built). The scheduling logic (recurrence, conflicts, free slots) is `ce-calendar`; wiring it to the server endpoints is still to do.
- barcodes: logic in `ce-barcode` (built).

Invariants (such as a booking's end after its start) are enforced by the server on create and patch (`ce_validate::check_invariants`). Computed fields are evaluated first (`ce_validate::evaluate_computed`: dependency order, decimals rounded half-up to the field's scale), so rules can read them. Rules that use `ref()` or read a computed sum over other records are skipped for now; `unenforced_invariants` lists them.

Second wave: invoicing is built; `ce-io` does CSV import and export (see its crate docs). Search is a list parameter, not an entity tool: mark string or text fields `"searchable": true` and `GET /v1/{inst}/{entity}?q=text` returns the records where any of them contains the text (case-insensitive, 1 to 100 characters, `%` and `_` literal), still tenant-scoped and combinable with `filter`, `sort` and paging. It scans one entity's rows (no full-text index yet); a cross-entity search endpoint comes later.
