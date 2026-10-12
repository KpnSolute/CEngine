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

## Not in this increment
Digest-pinned loader and install records (next), the `inventory` module, composing a plugin into the installed manifest, audit of plugin versions, upgrade and rollback, and failure-mode tests beyond descriptor validation.
