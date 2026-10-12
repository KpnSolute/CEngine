# CEngine languages and bindings (DRAFT v1)

Status: draft under Loom ADR-0007. Describes how Rust, Python and JavaScript /
TypeScript are used to build and consume the framework. No code is written or
authorized yet.

## Principle
**Semantics live once, in Rust.** Python and JS/TS never reimplement the
manifest validator, expression evaluator, query planner, policy compiler,
decimal arithmetic, canonical JSON or signing. They call the Rust core or a
generated artifact of it. Where a language must work without the core, it
conforms to the shared vectors and nothing else.

## Who does what
| Concern | Rust | Python | JS / TypeScript |
| --- | --- | --- | --- |
| Manifest parse, validate, hash | owns | calls via PyO3 | calls via WASM |
| Expression compile and evaluate | owns | calls | calls (browser pre-checks) |
| Query planner, tenant predicate | owns | never | never |
| Tenant scope (`TenantScope`) | owns; only buildable from a verified token | never builds one | never builds one |
| Postgres store, outbox, advisory locks | owns | never opens tenant DB connections | never |
| HTTP server (data, control, meta planes) | owns (axum, tokio) | none | none |
| Canonical JSON, decimals, signing, key rotation | owns | calls | calls |
| Typed client | none | generated | generated (primary) |
| Persona parsing, AI packaging, connector workers | none | owns (off hot path) | none |
| Forms, validation hints, admin UI | none | none | owns (uses the same WASM validator) |

## Rust: `ce-core` workspace
Crates (draft names):
- `ce-manifest`: parse, JSON Schema validation, normalisation, content hash.
- `ce-expr`: lexer, parser, type checker, evaluator and SQL compiler for the expression language.
- `ce-query`: filter/sort/include grammar to parameterised SQL. Always injects the tenant predicate.
- `ce-policy`: roles and entity/field rules compiled into the query.
- `ce-store`: direct Postgres (sqlx), transactions, version column, advisory locks, idempotency keys.
- `ce-events`: outbox, relay, CloudEvents, HMAC with 10-minute key overlap.
- `ce-provider`: provider trait toward Infajn plus a local no-op provider.
- `ce-server`: axum/tokio service exposing data, control and meta planes.
- `ce-py` (PyO3) and `ce-wasm` (wasm-bindgen): thin bindings, no logic.

Rules:
1. `TenantScope` has a private constructor reachable only from `verify_token`. Every store function takes a `&TenantScope`. A call without one does not compile.
2. Money and quantities use a decimal type (`rust_decimal` or an equivalent with 38 digits); no `f64` in any manifest-reachable path.
3. `unsafe` is forbidden outside bindings; each use is documented and reviewed.
4. Public API is semver'd. The manifest `manifest_version` and the wire `/ce/v1` move independently of crate versions.

Sketch of the stable surface (illustrative, not final):
```rust
pub fn compile(manifest_json: &[u8]) -> Result<CompiledTemplate, ManifestError>;
pub fn verify_token(token: &str, keys: &KeySet) -> Result<TenantScope, AuthError>;
pub async fn handle(req: Request, scope: &TenantScope, t: &CompiledTemplate) -> Response;
pub fn eval(expr: &CompiledExpr, env: &Env) -> Value;       // used by tests and bindings
```

## Python: `ce-py` and connectors
- Package `cengine` wraps `ce-py` (PyO3, abi3 wheels). Exposes `compile_manifest`, `validate_manifest`, `eval_expr`, `canonical_json`, `hash_manifest`.
- Generated client: `CEngineClient` from the manifest and OpenAPI, using `Decimal`, never `float`, for decimal fields; datetimes are timezone-aware UTC.
- Built (`ce-codegen`): `ce-codegen manifest.json --lang python --out client.py` writes one standard-library-only file (Python 3.11+). It has a dataclass per entity (`Decimal`, `date`, aware `datetime`, `Literal` enums), `Client(base_url, installation, token)` with `client.item.list/iter/get/create/update/delete`, filters and sorts built from the declared fields (`Item.f.unit_cost >= Decimal("2") & ~(Item.f.name == "Salt")`, `Item.f.name.desc()`), automatic idempotency keys on create, and `CEngineError` carrying the problem code. Floats are refused for decimals.
- Connector workers and AI/persona code consume events and call the API as a normal client with a service token. They never read or write tenant tables directly.
- Python is off the request hot path. If Python is unavailable, the data plane still serves.
- Typing: full type hints, `mypy --strict` clean on the public package.

## JavaScript / TypeScript: `ce-wasm` and the client
- Package `@kpnsolute/cengine` ships the WASM build (browser, Cloudflare Workers, Deno, Node) plus generated types.
- Client is generated per template from the manifest: entity types, action input types, query builder that only allows declared fields and indexed sort keys.
- Built (`ce-codegen`): `ce-codegen manifest.json --lang typescript --out client.ts` writes one dependency-free file that uses the global `fetch` (Node 18+, browsers, Deno, Bun) and only erasable TypeScript syntax, so Node 22.6+ and Deno run it directly. It has an interface per entity and a `...Input` type for creates (computed fields left out, required fields required), `Client({baseUrl, installation, token})` with `client.item.list/iter/get/create/update/delete` (`iter` is an async generator), field handles for filters and sorts (`and(ItemFields.unit_cost.gte("2"), not(ItemFields.name.eq("Salt")))`, `ItemFields.name.desc()`), automatic idempotency keys, and `CEngineError` with the problem code. Decimals are plain `string` aliases (`Decimal`), not branded types yet, and a fractional JavaScript number passed as a filter value is refused. No `any` in the generated entity code.
- Decimals are strings on the wire and branded string types in TypeScript; no `number` for money. A `Decimal` helper wraps WASM arithmetic.
- The same WASM validator runs in the form layer so browser and server reject identically.
- Strict mode, `noUncheckedIndexedAccess`, no `any` in generated code.

## Wire and canonical format (shared by all three)
- JSON, UTF-8, keys sorted for hashing and signing (RFC 8785 style), no insignificant whitespace.
- Decimals: JSON strings, canonical form with exact scale from the field. Ints: JSON numbers within 2^53, otherwise strings.
- Dates `YYYY-MM-DD`; datetimes RFC 3339 UTC with `Z`. Months are 1 to 12 everywhere.
- Money rounding: half-up, away from zero (see `expression-language.md`).
- Errors: RFC 9457 as in `catalogs/errors.json`. Each binding maps problem `code` to a typed error; unknown codes surface as a generic error carrying the code.

## Conformance (what keeps the three honest)
1. **Expression vectors** (`conformance/expression-vectors.json`): every binding must return the listed canonical output. The draft Python reference (`reference/expr_ref.py`) passes all 20 vectors; the Rust core must match it before it becomes the source of truth.
2. **Manifest vectors**: valid and invalid manifests with expected error codes (to be written).
3. **Golden request/response vectors** for the data plane, byte for byte across Rust, Python and TS clients (to be written).
4. **Tenant isolation suite**: a synthetic second tenant must never see or change data; SQL fuzzing asserts every generated statement carries the tenant predicate (to be written).
5. CI runs all suites on each binding; a mismatch blocks release.

## Build and release (draft)
- One repo (KpnSolute/CEngine) with a Cargo workspace, `bindings/python`, `bindings/wasm`, `client-ts`, `spec/`, `examples/`, `conformance/`.
- One version train for core and bindings; a core release publishes wheels and the npm package together.
- Consumption by Compute: pinned version through a private registry or tagged git dependency (open decision, due before Phase 2).

## Not decided
Rust edition/MSRV and Postgres version floor; whether the client generator is Rust or TypeScript; WASM handler runtime (wasmtime vs another); Python minimum version (suggest 3.11); Node/Deno support matrix.
