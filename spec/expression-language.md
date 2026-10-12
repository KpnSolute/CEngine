# CEngine expression language (DRAFT v1)

Status: draft under Loom ADR-0007. Used by `computed`, `invariants[].expr`,
`locked_when`, `projections[].filter` and `effect.set` values in manifests.

## Design rules
1. **Pure and total.** No side effects, no loops, no recursion, no user functions. Every expression terminates.
2. **Deterministic.** Same inputs give the same output in Rust, Python and TypeScript. Time and randomness are never read inside an expression; `now` and `today` are bound once per transaction.
3. **Exact numbers.** Decimals are exact base-10 values, never binary floats. A `decimal` field compared or computed with another decimal never loses precision before the final rounding.
4. **Compiles to SQL where possible.** Anything used in `filter` or `computed` must compile to a set-based Postgres expression; the planner rejects an expression it cannot compile.
5. **Bounded.** Max 2,000 characters, depth 32, 256 nodes. Over limit is a publish-time error.

## Types
`bool`, `int` (64-bit), `decimal` (up to 38 digits, scale up to 18), `string` (UTF-8), `date`, `datetime` (UTC), `null`, `list<T>` (only for `in` and aggregate arguments). No implicit conversion between `int`, `decimal` and `string`; `int` widens to `decimal` only.

## Grammar (EBNF)
```
expr      = or ;
or        = and { "or" and } ;
and       = not { "and" not } ;
not       = [ "not" ] cmp ;
cmp       = add [ ( "==" | "!=" | "<" | "<=" | ">" | ">=" | "in" ) add ] ;
add       = mul { ( "+" | "-" ) mul } ;
mul       = unary { ( "*" | "/" | "%" ) unary } ;
unary     = [ "-" ] postfix ;
postfix   = primary { "." IDENT | "(" [ args ] ")" } ;
primary   = NUMBER | STRING | "true" | "false" | "null"
          | IDENT | "(" expr ")" | "[" [ args ] "]" ;
args      = expr { "," expr } ;
IDENT     = [a-z][a-z0-9_]* ;
NUMBER    = digits [ "." digits ] ;
STRING    = "'" { any char except "'" or "''" } "'" ;
```
String quote is escaped by doubling (`'it''s'`). There is no `if` keyword; use `when(cond, a, b)`.

## Bindings
| Name | Meaning |
| --- | --- |
| field names | Fields of the current entity row, after the pending write is applied |
| `old.field` | Value before the write (null on create) |
| `actor.id`, `actor.role` | From the verified token |
| `now`, `today` | Transaction start time (UTC), fixed for the whole transaction |
| `ref(field)` | The row referenced by a `ref` field (one hop only); fields read as `ref(period).state` |

`tenant`, `installation` and any slug are **not** bindable. Templates cannot see them.

## Null semantics
Three-valued logic. `null == null` is **true**; any other comparison with null is null. Arithmetic with null is null. `and`/`or` follow SQL three-valued rules. A null result of an `invariant` counts as **failure**; a null `locked_when` counts as **not locked**; a null `filter` excludes the row. Use `coalesce` to be explicit.

## Arithmetic
- `+ - *` on decimals are exact. `/` returns a decimal with scale 18, rounded half-up (away from zero on ties). Division by zero yields null and records `expr.div_zero` in the audit log; it never raises.
- `%` is integer-only. Overflow past 64 bits or 38 digits is a validation failure (`validation_failed`, code `expr.overflow`), not a wrap.
- `round(x, n)` rounds half-up (away from zero) to `n` places, `0 <= n <= 18`. Money fields use `round(..., 2)`.

## Functions (complete list)
| Function | Result |
| --- | --- |
| `round(d, n)`, `floor(d)`, `ceil(d)`, `abs(x)` | numeric |
| `min(a, b)`, `max(a, b)` | numeric or date |
| `coalesce(a, b, ...)` | first non-null |
| `when(cond, a, b)` | `a` if cond is true, else `b` (null cond picks `b`) |
| `len(s)`, `lower(s)`, `upper(s)`, `trim(s)`, `concat(a, b, ...)` | string |
| `starts_with(s, p)`, `contains(s, p)` | bool |
| `is_null(x)` | bool |
| `days_between(a, b)` | int (b minus a, whole days) |
| `add_days(d, n)`, `year(d)`, `month(d)` (1 to 12), `day(d)` | date or int |
| `sum(list)`, `count(list)` | aggregates, **only inside `computed` on a parent over `children(entity, field)`** |
| `children(entity, ref_field)` | rows of a child entity pointing here (aggregate source) |

Aggregates take exactly `children(entity, field)` or `children(entity, field).attr`. `sum` skips
nulls, and the sum of no rows is `0`; it is an int unless any value is a decimal. `count(children(e, f))`
counts rows, `count(children(e, f).attr)` counts rows where `attr` is not null. Deleted rows are
not children. `ref(field).attr` reads one attribute of the referenced row, and is null when the ref
is null or the row is gone. Evaluation needs the store; the pure evaluator reports `Aggregate`.
A failed lookup fails the write, it never counts as zero or passes a rule.

Months are **1 to 12** everywhere. The 0-indexed month storage seen in MJCC is
never exposed through the language.

Adding a function is additive (minor version). Changing semantics of an existing
one is a major-version break.

## Where each form may appear
| Site | Allowed | Notes |
| --- | --- | --- |
| `computed` | arithmetic, string, `when`, `coalesce`, `sum`/`count` over `children` | Re-evaluated in the write transaction. Cycles are a publish-time error. |
| `invariants[].expr` | any non-aggregate | Evaluated in the same transaction; failure rolls back with `validation_failed` and the invariant's `message_code`. |
| `locked_when` | any non-aggregate, may use `ref()` | Failure to evaluate counts as unlocked. |
| `filter` (projections) | comparisons, `and`/`or`/`not`, `in`, string tests | Must compile to SQL. No `ref()`, no aggregates. |
| `effect.set` values | any non-aggregate | May read `actor.*` and `old.*`. |

## Errors at publish time (reject manifest)
Unknown identifier; type mismatch; expression too large; computed-field cycle;
`filter` that cannot compile to SQL; use of a forbidden binding; a function
outside its allowed site.

## Handlers
Anything this language cannot express goes to a sandboxed WASM handler
(`actions[].handler`). A handler is reviewed per template, has no network or
filesystem access, a fuel limit and a memory limit, and may only read and write
through the same transactional store API as actions.

## Examples (taken from the draft templates)
- `round(on_hand * unit_cost, 2)`
- `on_hand >= 0`
- `ends_at > starts_at`
- `ref(period).state == 'published'`
- `when(amount > 1000, 'director', 'manager')`

## Actions over HTTP
`POST /v1/{inst}/actions/{name}` with body `{"id": "<record>", "input": {...}}`, `If-Match` (the record's version) and `Idempotency-Key`. The caller's role must list the action (`roles.*.actions`); that alone is enough, the role needs no write on the entity. The effects run in order inside one transaction; any failure undoes all of them:

- `set`: each value is an expression, evaluated against the record as it stands and written through the normal write path (rules, computed fields, `locked_when` and audit apply). Bindings: the record's fields, `input.<name>`, `actor.*`, `now`, `today`.
- `transition`: moves the record to the named state if the state graph allows it from the current one (409 `invalid_transition` otherwise).
- `create`: builds a record of another entity from expressions, with its initial state and defaults, and creates it. Its idempotency key is derived from the request's key.
- `emit`: names an event. It is returned in `meta.emitted` and handed to the outbox when events out lands.

`requires_state` is checked first (409 `invalid_transition`); the input is validated against the action's `input` fields (422). The response is the record after the last effect plus `meta.created` and `meta.emitted`. A retry of a finished action fails with 409 `version_conflict` because the record has moved on.

## Batch writes
`POST /v1/{inst}/_batch` with `{"ops": [...]}` (1 to 100) and an `Idempotency-Key`. Each operation is `{"op": "create", "entity", "data"}`, `{"op": "update", "entity", "id", "version", "data"}` (a merge patch) or `{"op": "delete", "entity", "id", "version"}`. All operations are checked (roles, shape, server-only records) before anything is written, then applied in order in one transaction with audit; any failure undoes all of them and returns that operation's error. Later operations see the earlier ones. Governed entities are refused for callers who cannot merge. The response is `{"data": [record or null, ...]}` in the order sent. Creates derive their idempotency keys from the request's key.
