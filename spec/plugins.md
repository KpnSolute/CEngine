# Plugins (draft v1)

Status: draft. Implements the first increment of the plan Loom recorded in `decisions/CEngine-MJCC-plugin-answers-proposal.md` (Loom PR #7, owner decision pending on the MJCC parts). Nothing here loads, installs or runs a plugin yet; `ce-plugin` only reads and validates the descriptor.

## What a plugin is in 0.x
A customer-specific **policy pack**: a `plugin.toml` that fills the policy slots of one registered module with parameters and expressions in the CEngine expression language. It is data. There is no code execution, no file or network access, and no host other than `policy-pack`; `isolated-python`, WASM and any other kind are refused with `unsupported_host` until a separate ABI and sandbox proposal is approved.

## Descriptor
Located at `.cengine/plugins/<slug>/plugin.toml` (see `customer-plugin-package-convention.md`). Fields:

- `schema_version` = 1.
- `[plugin]` `id` (lowercase slug, 63 max), `version` (semver), `description`.
- `[compatibility]` `engine` (semver range the running engine must satisfy), `module` (registered module id), `module_api` (must equal the module's API major).
- `[execution]` `kind = "policy-pack"`.
- `[capabilities]` `read`, `write`, `emit` lists, each entry must be offered by the module; `external_network` must be false (provider calls belong to Infajn).
- `[[policy]]` `slot`, optional `rule`, optional `params`. A slot is filled at most once; the module decides whether a slot requires a rule, forbids one, and which parameter names it accepts. Rules must parse as CEngine expressions. Parameter values are strings, integers, booleans, arrays or tables; floats and datetimes are refused so money and quantities are decimal strings or integers.
- `[tests]` `fixtures`: relative path inside the plugin folder. Absolute paths, `..`, `.`, empty parts, backslashes and drive prefixes are refused.

Unknown fields are errors. The descriptor is limited to 64 KiB and 64 policies. The reader collects every violation with a code and a field path.

## What the registry controls
Module ids, API majors, slot names and shapes, and the capability vocabulary live in CEngine (`ce_plugin::Registry`), never in the plugin. A plugin cannot invent a hook or widen its own permissions.

## Loading and pinning
`ce_plugin::load(root, slug, pin, registry, engine)` reads `<root>/.cengine/plugins/<slug>/plugin.toml` from a local folder, never from the network and never from a Git HEAD. A `Pin` is what an installation approved: plugin id, exact version and `sha256:` digest of the descriptor bytes. The loader:

- accepts only safe slugs, and only regular files under real directories; the file is opened without following a symlink and checked on the opened handle (a swapped parent directory can at worst make the load fail, because bytes that do not hash to the pin are never accepted);
- refuses files over the descriptor limit before hashing;
- hashes the same bytes it then parses, so the file cannot change between check and use;
- refuses a mismatched digest (`DigestMismatch`), then validates the descriptor, then requires the folder name, `plugin.id` and `plugin.version` to match the folder and the pin.

`discover(root)` lists plugin folders (real directories with safe names) without reading them; `digest_of` gives an approver the digest of what is on disk. `Installed` keeps the approved pins per plugin with history: `approve` makes a pin active, `rollback` steps back to the previously active pin (never leaving a plugin with none) while the approval log stays append-only, `disable` stops serving a plugin without forgetting it. It is plain data; persisting it belongs to the host store.

A customer-editable lockfile is never proof of approval: the pin comes from the trusted installation record, not from the repository.

## Modules and composition
A module is a manifest fragment with named policy slots (`crates/ce-plugin/modules/*.json`). `business.inventory` (API 1) adds `stock_item`, `stock_line` and `stock_movement` (plus the price-observation and invoice entities below) over the `period` tool, with these slots: `price`, `ending_qty`, `opening_value`, `received_value`, `pulled_value`, `ending_value` (computed-field expressions) and `over_pull` (the line invariant; `true` allows negative stock). The line's price is the one stamped on it when it was created; a line never reads the item's current price, so a later change to the item cannot move an existing line. Every slot has a module default, so a template with no plugin still gets a working module.

`ce_plugin::compose(template, module, plugin)` puts the module into a template, replaces each slot with the plugin's rule when it filled one, and validates the result with `ce-manifest`. A rule that does not type-check is refused at composition (install time), never at request time. It refuses an unknown module, a plugin written for another module or API major, a template missing a required entity (`period`), and an entity name the template already uses. The template's roles are never touched. The returned manifest has the usual canonical hash.

The same module with two different plugins gives two different, correct behaviors with no engine change; `crates/ce-server/tests/plugins.rs` runs both through the HTTP API on Postgres (a pull that overdraws a line is refused under the defaults and accepted under a lenient plugin that also prices to four places).

## Provenance in the audit trail
`compose` takes the plugin together with the digest it was pinned to and writes a `provenance` block into the composed manifest: the module name and API, and the plugin id, version and `sha256:` digest (no plugin block when the defaults ran). Because it is part of the manifest, it also changes the manifest hash. With the `audit` tool, every audit event stores `manifest_hash` and `provenance`, so a record can be traced to the exact rules that were in force when it changed. `ce_manifest::validate` refuses a manifest that carries `provenance` (`provenance_reserved`), so a hand-written file cannot forge it; only `validate_composed`, called by `compose` and by a trusted host on its output, accepts it. A composed manifest therefore cannot yet be loaded from a file by `ce-serve`; that arrives with persisted installation records.

## Rollover
The inventory module ships a server action for this: `POST /v1/{inst}/actions/rollover_stock` with body `{"id": "<closed period>", "input": {"to": "<next period>"}}` carries every line in one transaction (quantity, cost per unit at six places, raw price); the period must be closed (409 otherwise), and a second run is refused. The client procedure below still works and is what the action replaces for UIs.

Rollover (next period's opening = prior period's ending) needs no further engine feature. A client reads the closed period's lines, builds one `create` per line with `opening_oh` set to the old `ending_qty` and `unit_price` to the price going forward (the old line's raw `unit_price` when it has not changed), plus `opening_unit_cost` when the plugin carries cost (see "Over-pull rule and cost carried at rollover"), and sends them through `_batch` in chunks (at most 100 operations each). Each chunk carries its own `Idempotency-Key`, so repeating a chunk after a lost response creates nothing twice. `crates/ce-server/tests/plugins.rs` (`rollover_carries_each_ending_...`) runs this on Postgres: five items in two-line chunks, run twice, five lines, openings equal to the prior endings, the closed period untouched. The client carries the old line's raw `unit_price`, not its computed `price`, so a plugin's price rule is applied once to the new line and is not required to be idempotent. A second run under a new key conflicts on the unique `line_key` (item and period) instead of duplicating. A negative ending (possible only under a plugin that allows over-pulling) cannot be carried: `opening_oh` is never negative, so the batch is refused whole; the shortfall must be settled in the closing period first.

## Installing and running a plugin (`ce-serve`)
The approval record is plain JSON (`Installed::to_json` / `from_json`): every pin ever approved per plugin (append-only), which positions were active (for rollback) and whether the plugin is disabled. Reading is strict: an unknown field, bad plugin id, malformed digest or version, or an `active` position outside the log refuses the whole file. `ce-serve` writes it through a temporary file and a rename.

Owner commands (they need `CE_PLUGIN_ROOT` and `CE_INSTALLS`, no database):

```text
ce-serve plugin digest <slug>             print the digest of what is on disk
ce-serve plugin approve <slug> <digest>   approve it; refuses unless the disk still has that digest
ce-serve plugin status | rollback <slug> | disable <slug> | enable <slug>
```

To run a template with a plugin, set `CE_TEMPLATE` (a template JSON, not a finished manifest), `CE_MODULE`, `CE_PLUGIN`, `CE_PLUGIN_ROOT` and `CE_INSTALLS` instead of `CE_MANIFEST`. Both `ce-serve install` and `ce-serve` call `ce_plugin::resolve`, so the schema and the served rules come from the same approved bytes. It fails closed: a plugin that was never approved or is disabled, a descriptor edited after approval, a missing folder, or a rule that does not type-check each stop the server with a message; none falls back to the defaults. Without `CE_PLUGIN` the module runs on its defaults. Rolling back needs the older version's bytes back on disk, because the digest pin is checked against the file.

Tests: `crates/ce-plugin/tests/install.rs` (record, resolve, failure modes) and `crates/ce-serve/tests/plugin_cli.rs` (the binary, including `install` against Postgres).

## Carried opening cost and legacy parity
`stock_line` has an `opening_unit_cost` field (default 0): the unit cost the opening quantity was carried in at. Module defaults ignore it (opening is valued at the line price), so existing installations are unchanged. A rollover client sets it to the closing cost per unit (closing value divided by closing quantity, six decimal places), so a later price change in the new period does not revalue the opening stock; the exact derivation is in "Over-pull rule and cost carried at rollover". A plugin that wants the carried cost fills the `opening_value` slot with a rule that reads it.

`crates/ce-plugin/tests/fixtures/mjcc-legacy-valuation/plugin.toml` is a synthetic copy of MJCC's valuation rules as the legacy Compute backend applies them today (opening at carried cost, falling back to price; ending value follows quantity and never goes below zero). `crates/ce-plugin/tests/legacy_parity.rs` runs 164 synthetic rows from `tests/parity/mjcc_inventory_vectors.json` through the composed manifest: quantities and every money figure match the legacy results to the cent, except four figures on two exact half-cent products, listed below.

Rounding: CEngine rounds exact decimals half up, as the legacy Postgres trigger does (`round(numeric, 2)`). The legacy Python read path rounds binary floats, so an exact half cent can land a cent lower (`1.005 x 1` gives 1.00, `0.045 x 1` gives 0.04). The vectors show where the two differ; the stored database value agrees with CEngine. Treat the float result as a legacy artifact unless the owner says otherwise.

The generator (`tests/parity/gen_mjcc_inventory_vectors.py`) imports the legacy formulas read-only from a local Compute checkout and uses synthetic inputs only.

## Price observations
`business.inventory` also holds the observed-price protocol the legacy backend applies to invoice prices. A `price_observation` (line, observed price, source) is settled by the `settle_price_observation` action: it records the line's current price, decides with the `price_decision` slot, and in the same transaction either applies the price to the line (`auto_applied`), holds it (`pending`) or drops it (`dismissed`). The default rule: no usable price (zero or less) is `skip`; a line with no current price takes it (`apply`); a difference under half a cent is `same`; a move of more than 25% of the current price is `hold`; anything else is `apply`. The comparison is exact decimal arithmetic (`abs(new - old) > 0.25 * old`), so exactly 25% applies. The first effect takes the line's lock (it rewrites the line's price with itself), so two observations for one line settle one after the other and each decides from the price the previous one left. A negative price is refused when the observation is created (`stock.negative_price`), so it never reaches the decision; the `skip` branch covers zero. A manager resolves a held observation with `apply_price_observation` (optionally with their own price) or `keep_price_observation`; a decided observation cannot be decided again. Lines of a closed period are frozen, so an observation cannot move their price. Who may run each action is the template's role list, not the module's.

`crates/ce-plugin/tests/legacy_parity.rs` runs 326 synthetic price pairs generated from the legacy `decide_price` (`tests/parity/gen_mjcc_price_vectors.py`). All decisions match except six moves of exactly 25%, where the legacy float arithmetic lands a hair above 0.25 and holds them; the exact rule applies them, as the legacy tests and comments say it should. They are pinned by name in the test.

## Invoices

`business.inventory` also holds supplier invoices, so a delivery can be recorded once, reviewed, and applied to stock. Four entities, all `governed` (a caller who cannot merge stages the write in a changeset; see `spec/tools.md`): `supplier_invoice`, `supplier_invoice_line`, `sku_alias` and `price_observation`.

- `supplier_invoice` (named so it can sit beside the `invoicing` tool's customer `invoice`): vendor, number, date, `fuel_charge`, `tax`, `vizient_discount`, the printed `stated_total`, and the document's name and SHA-256. Computed: `invoice_key` (lower-cased, trimmed vendor and number, **unique**, so the same invoice cannot be recorded twice whatever its case or spacing), `goods_total` (sum of the lines), `net_total` (goods + fuel + tax - discount), `delta` (printed - net) and `reconciled` (`abs(delta) < 0.01`). An invoice that does not add up is stored and flagged, not refused.
- `supplier_invoice_line`: invoice, line number, vendor SKU, quantity (> 0), unit price, computed `ext_price`, and an optional `item`. A line with no `item` is a product the kitchen does not know yet: filter `item` empty to get the review queue. `sku_alias` (vendor + vendor SKU, unique, to an item) records the answer.
- Applying an invoice is a set of ordinary writes in **one changeset**: the invoice (with a chosen `id`, see `spec/expression-language.md`), its lines, one `stock_movement` per line with `received` set, and one `price_observation` per priced line. The reviewer merges them all in one transaction or none. After the merge the reviewer runs `settle_price_observation` per observation (actions cannot be staged). `crates/ce-server/tests/invoice_intake.rs` runs this on Postgres: stage, a clerk cannot merge, merge, totals and the unknown-SKU line, the stock received, the price settle, the repeated invoice refused with no second receipt, and an unreconciled invoice flagged.
- To make the received stock part of the review, a template marks `stock_movement` `governed` as well (the module leaves it open so daily pulls are not forced through review). Until it does, a staff role with write access to `stock_movement` can write a receipt directly; the template decides.

## Over-pull rule and cost carried at rollover

Two legacy MJCC behaviours need no engine change; they are policy and client procedure, both proved on Postgres in `crates/ce-server/tests/plugins.rs`.

- **Existing shortfalls stay editable, but never grow.** The `over_pull` slot can read `old.*`, so a plugin can write `opening_oh + received - pulled >= 0 or opening_oh + received - pulled >= coalesce(old.opening_oh, 0) + coalesce(old.received, 0) - coalesce(old.pulled, 0)` (with `ending_qty` set to `opening_oh + received - pulled` so a shortfall shows as negative). A line that is already short can have its pull lowered or an unrelated field changed; raising the pull, or creating a new short line, is refused with `stock.over_pull` (test `an_existing_shortfall_may_shrink_but_never_grow_under_the_legacy_over_pull_rule`, which opens history written under a permissive policy and then serves it under this rule).
- **Rollover carries cost.** For each closed line with stock on hand the client sends `opening_oh` = closing `ending_qty`, `opening_unit_cost` = closing `ending_value` / `ending_qty` rounded to six places, and `unit_price` = the price going forward. A line with nothing on hand (`ending_qty` of 0, where the ratio is undefined) sends `opening_oh` 0 and no `opening_unit_cost`. Under the legacy valuation rules the opening stock keeps its carried cost, so a supplier's later price rise does not revalue what was already on hand (test `rollover_carries_cost_so_a_new_price_does_not_revalue_what_was_on_hand`: 6 on hand worth 8.57, carried at 1.428333, stay worth 8.57 after the price moves to 3, and a depleted line rolls over at 0). **Rounding:** the cost is stored to six places, so the carried value can differ from the closing value by up to half of one millionth of a currency unit per unit of stock (for example 87,654.321 units worth 1,047,086.42 carry at 11.945634 and open at 1,047,086.44). The legacy system has the same limit only if its carried cost is stored to six places; this needs the owner's acknowledgement before parity is claimed for very large lines.

## Plugin-owned entities (tables)

A plugin may add entities of its own, the way a Supabase project adds tables, but only as data the engine understands, never as SQL. In `plugin.toml`:

```toml
[[entity]]
name = "limit"
[entity.definition.fields.label]
type = "string"
required = true

[[entity]]
name = "check"
[entity.definition.fields.limit]
type = "ref"
ref = "limit"
```

- `definition` is an ordinary manifest entity (fields, state, invariants, `governed`, ...). `ce-manifest` validates it when the plugin is composed, so a bad type or a `ref` to an unknown entity is refused at install time, not at request time.
- Names: `name` is lowercase letters, digits and single underscores, up to 30. The entity is installed as `<plugin id with hyphens as underscores>__<name>` (plugin `food-safety`, entity `limit` becomes `food_safety__limit`). A `ref` to another of the plugin's own entities is rewritten to the namespaced name; a `ref` to a module or template entity is left as written.
- A plugin cannot replace or shadow an existing entity: a name already in the template is `Clash`.
- Limits: 16 entities per plugin; numbers follow the rule for parameters (decimal strings or integers, no floats).
- Everything else comes from the engine as for any entity: `tenant_id` on every row, roles, audit with the plugin digest, changesets for `governed` entities. The template's roles are not changed, so who may read or write a plugin's entities is still the template's decision.
- Because the entities are part of the composed manifest, they change its hash and are pinned with the plugin digest.

Not in this increment: plugin-defined routes and actions, event subscriptions, connector settings, what happens to a plugin's tables when it is disabled or removed (`ce-migrate` emits no `DROP` today, so data is kept, but this is not yet a tested guarantee), and a migration step for upgrading a plugin's entities across versions.

## Not in this increment
Fetching plugin folders from a customer repository (the host places the folder), a plugin release approval workflow beyond the owner commands, and the HTTP deploy.
