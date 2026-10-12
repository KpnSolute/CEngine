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

## Not in this increment
Persisting installation records, rollover, price review, and failure-mode tests beyond descriptor and composition validation.
