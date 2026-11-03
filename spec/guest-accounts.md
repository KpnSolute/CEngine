# Guest accounts, guest resources and role-restricted projections

Status: implemented (0.2.x). Domain-neutral: nothing here names a company, a product or a form. A template
opts in with a `guest` block; a host opts in with `CE_GUESTS=postgres`.

## What it is for

People who are not staff (parents, visitors, customers) need an account, need to send a few things in
and see their own answers, and must never reach staff data. This is a second door beside staff PIN
login, not a new kind of staff.

## Accounts (`/v1/{inst}/_guest/...`)

| Call | Body | Answer |
| --- | --- | --- |
| `POST signup` | `name`, `email`, `password` | 201 `{data:{token, expires_at, name}}`; 409 `email_taken`; 422 with a reason per field |
| `POST login` | `email`, `password` | 200 `{data:{token, expires_at, name}}`; 401 `invalid_credentials` (the same for a wrong password, an unknown email and a malformed one); 429 `throttled` |
| `POST logout` | (guest token) | 204; the guest's older tokens stop working |
| `POST reset` | `email` | 202 `{data:{sent:true}}` whether or not the account exists; the email carries a one-hour, single-use link |
| `POST reset/confirm` | `token`, `password` | 200; sets the password and ends every earlier session |

Security model:

- Passwords are scrypt-hashed (`kpn-scrypt`, the staff PIN format, 16 MiB per check, at most 8 at once). 8 to 64
  characters, not one repeated character, not the email. The engine stores no password, no reset token (only its
  SHA-256), and logs neither.
- Sign-in, sign-up and reset are rate limited in process: 8 wrong passwords per email and 40 per address per
  15 minutes, 10 sign-ups per address per hour, 3 resets per email per 15 minutes. An unknown email costs the same
  scrypt work as a known one.
- A guest token carries one role, the manifest's `guest.role`, and a subject `guest:<uuid>`. The manifest check
  refuses a guest role that holds any read, write or action permission, so a guest token opens no staff route
  even if it is replayed there. Staff tokens are refused on guest routes.
- Sign-out and password reset end earlier sessions (`not_before`, checked on every request from memory, warmed
  from `_ce_guest` at start). A token issued in the same second as a cutoff is issued one second after it.
- Tables `_ce_guest` and `_ce_guest_reset` are created in the installation's schema on first start, like `_ce_staff`.

## Guest resources (`guest.resources`)

A resource is rows of one entity that a guest may create and list, only their own. The guest never names the
entity, the owner or the state.

- `owner_field`: a string field (at least 64 characters) that holds `guest:<uuid>`. Every list is filtered by it
  and by `fixed`.
- `fixed`: fields the server sets on every row (and filters by).
- `stamp`: date fields set to today (UTC). `created_at_field` names the field shown to the guest as `created_at`.
- `title` / `title_field`: an expression over the body (snake_case names) that fills a title field.
- `body.field`: the json field that holds what the guest sent; `body.as` is the key it arrives and leaves under.
- `body.allow`: the only keys a guest may send. Everything else is dropped. Values are text (at most 2000
  characters), whole numbers or true/false.
- `body.required`, `body.rules` (expressions that must be true, each with a message and the field to blame),
  `body.derive` (server-computed keys; a guest cannot send them) and `body.max_bytes`.
- `body.state` (`key`, `initial`) and `body.note_key`: the lifecycle lives in the same json, set by the server on
  create and changed by staff through the normal write path. The guest sees both, never who decided.

Rule and derive expressions see the allowed keys in snake_case (`eventDate` is `event_date`), plus `today` (a
`YYYY-MM-DD` string) and `now`. The date functions `add_days` and `days_between` are in the expression
language spec but not yet in the evaluator, so compare ISO date strings directly (`event_date >= today`).

Writes go through the same validation, entity rules and audit as any other write, with the guest as the actor,
and at most 20 per guest per hour.

The guest's view of a row is `{id, <state key>, <note key>, created_at, <as>: {allowed and derived keys}}`.

## Public reads (`guest.public`)

`GET {name}` with no token returns `{data: <value_field>}` of the row where `match.field == match.value`
(`{}` when there is none). Fields marked `pii` are refused.

## Role-restricted projections (`projections.<name>.roles`)

A projection with `roles` is not public. Signed-in callers holding one of those roles read it at
`GET /v1/{inst}/_view/{projection}` (same fields, filter and paging rules as a public projection, the caller's
own tenant). This is how a role gets a scoped read of one record without read access to the whole entity.

## Host settings

`CE_GUESTS=postgres`, `CE_TENANT`, `CE_GUEST_TTL_SECS`, and for password reset `CE_RESEND_API_KEY`,
`CE_MAIL_FROM`, `CE_GUEST_RESET_URL` (https). Without the key a reset request answers 503. The key is never
printed.

## Not here

Meal tickets, surveys that the engine runs itself, and POS receipts are separate pieces of work. Email
verification of a new address is not done: an account works as soon as it exists, and a reset email only goes
to the address on the account.
