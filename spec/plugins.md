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
A module is a manifest fragment with named policy slots (`crates/ce-plugin/modules/*.json`). `business.inventory` (API 1) adds `stock_item`, `stock_line` and `stock_movement` over the `period` tool, with these slots: `price`, `ending_qty`, `opening_value`, `received_value`, `pulled_value`, `ending_value` (computed-field expressions) and `over_pull` (the line invariant; `true` allows negative stock). The line's price is the one stamped on it when it was created; a line never reads the item's current price, so a later change to the item cannot move an existing line. Every slot has a module default, so a template with no plugin still gets a working module.

`ce_plugin::compose(template, module, plugin)` puts the module into a template, replaces each slot with the plugin's rule when it filled one, and validates the result with `ce-manifest`. A rule that does not type-check is refused at composition (install time), never at request time. It refuses an unknown module, a plugin written for another module or API major, a template missing a required entity (`period`), and an entity name the template already uses. The template's roles are never touched. The returned manifest has the usual canonical hash.

The same module with two different plugins gives two different, correct behaviors with no engine change; `crates/ce-server/tests/plugins.rs` runs both through the HTTP API on Postgres (a pull that overdraws a line is refused under the defaults and accepted under a lenient plugin that also prices to four places).

## Provenance in the audit trail
`compose` takes the plugin together with the digest it was pinned to and writes a `provenance` block into the composed manifest: the module name and API, and the plugin id, version and `sha256:` digest (no plugin block when the defaults ran). Because it is part of the manifest, it also changes the manifest hash. With the `audit` tool, every audit event stores `manifest_hash` and `provenance`, so a record can be traced to the exact rules that were in force when it changed. `ce_manifest::validate` refuses a manifest that carries `provenance` (`provenance_reserved`), so a hand-written file cannot forge it; only `validate_composed`, called by `compose` and by a trusted host on its output, accepts it. A composed manifest therefore cannot yet be loaded from a file by `ce-serve`; that arrives with persisted installation records.

## Rollover
Rollover (next period's opening = prior period's ending) needs no engine feature. A client reads the closed period's lines, builds one `create` per line with `opening_oh` set to the old `ending_qty` and `unit_price` to the old `price`, and sends them through `_batch` in chunks (at most 100 operations each). Each chunk carries its own `Idempotency-Key`, so repeating a chunk after a lost response creates nothing twice. `crates/ce-server/tests/plugins.rs` (`rollover_carries_each_ending_...`) runs this on Postgres: five items in two-line chunks, run twice, five lines, openings equal to the prior endings, the closed period untouched. The client carries the old line's raw `unit_price`, not its computed `price`, so a plugin's price rule is applied once to the new line and is not required to be idempotent. A second run under a new key conflicts on the unique `line_key` (item and period) instead of duplicating. A negative ending (possible only under a plugin that allows over-pulling) cannot be carried: `opening_oh` is never negative, so the batch is refused whole; the shortfall must be settled in the closing period first.

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
`stock_line` has an `opening_unit_cost` field (default 0): the unit cost the opening quantity was carried in at. Module defaults ignore it (opening is valued at the line price), so existing installations are unchanged. A rollover client sets it to the closing line's price, so a later price change in the new period does not revalue the opening stock. A plugin that wants the carried cost fills the `opening_value` slot with a rule that reads it.

`crates/ce-plugin/tests/fixtures/mjcc-legacy-valuation/plugin.toml` is a synthetic copy of MJCC's valuation rules as the legacy Compute backend applies them today (opening at carried cost, falling back to price; ending value follows quantity and never goes below zero). `crates/ce-plugin/tests/legacy_parity.rs` runs 164 synthetic rows from `tests/parity/mjcc_inventory_vectors.json` through the composed manifest: quantities and every money figure match the legacy results to the cent, except four figures on two exact half-cent products, listed below.

Rounding: CEngine rounds exact decimals half up, as the legacy Postgres trigger does (`round(numeric, 2)`). The legacy Python read path rounds binary floats, so an exact half cent can land a cent lower (`1.005 x 1` gives 1.00, `0.045 x 1` gives 0.04). The vectors show where the two differ; the stored database value agrees with CEngine. Treat the float result as a legacy artifact unless the owner says otherwise.

The generator (`tests/parity/gen_mjcc_inventory_vectors.py`) imports the legacy formulas read-only from a local Compute checkout and uses synthetic inputs only.

## Not in this increment
Price review, fetching plugin folders from a customer repository (the host places the folder), a plugin release approval workflow beyond the owner commands, and the HTTP deploy.
