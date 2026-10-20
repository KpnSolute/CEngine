# Brief for Claude: finish the MJCC backend on CEngine by Wednesday 2026-10-21

**From:** owner / senior engineer on CEngine  
**To:** Claude (engineering lead on `KpnSolute/CEngine` and `KpnWorld/MJCC-workspace`)  
**Date issued:** Monday 2026-10-19  
**Hard deadline:** Wednesday 2026-10-21 end of day: the **entire MJCC backend** runs on CEngine in staging, proven through the real HTTP API. Thursday we start the UI.

Read this whole file before touching code. It replaces every earlier plan that conflicts with it. Copy it to `CEngine/spec/mjcc-backend-complete-by-wednesday.md` so both repositories carry the same instructions, and keep both copies in sync when you amend it.

---

## 1. What we are building, in one paragraph

**CEngine is not MJCC.** CEngine is KpnSolute's proprietary, metadata-driven runtime (Rust, 24 crates) that executes *any* business template: a manifest declares entities, fields, states, invariants, roles, actions, reports and projections; the engine supplies storage, tenant isolation, auth, audit, changesets, events, files, barcodes, calendar and the HTTP API; **modules** add reusable business entity packs (today: `business.inventory`); **plugins** add a tenant's private rules and formulas (pricing, approvals, valuation) as digest-pinned policy packs. Think of it the way Frappe Framework relates to ERPNext: Frappe is the DocType engine with permissions, workflow and REST generated from metadata; ERPNext is one application built on it. CEngine is the engine. **MJCC is a template**: `template/mjcc.template.json` + `plugin/.cengine/plugins/mjcc-inventory` + fixtures, installed for one tenant (Miami Job Corps Center). Future KpnSolute templates for other industries use the same engine and the same mechanisms.

**MJCC is not an inventory app.** It is the daily operations workspace for the Food Service department, and it is designed to grow into the whole center the way GuideWell's internal portal serves all of Florida Blue: one sign-in, one role model, one calendar, one task flow, one document store, one certification tracker, one cost view, with department packs (food service first) plugged in underneath. Build the template in two layers from the start:

- **`workspace.core`** (department-agnostic, reusable by every future tenant): staff, roles and scopes, tasks and assignments, calendar and shifts, checklists, documents and file vault, notifications, certifications and training, equipment and assets, incidents, budgets vs actuals, audit, changesets, reports.
- **`foodservice`** pack (MJCC's department): three-week inventory, receiving, invoices and price review, HACCP, menus and meal periods, meal counts and waste, snack bar POS, purchasing and par levels, recipes (later).

The food-service pack depends on core; core never depends on food service.

---

## 2. Ground rules (unchanged, non-negotiable)

1. **Live MJCC is untouched until the new one is confirmed.** Supabase project `mgvyylvmkxhhataavqjz` (MJCCv1, JobCorp org) is **read-only**. Use the `supabase` MCP tool `supabase_query` on it; never `supabase_execute_sql`. The old Compute app stays the system of record until the new one is confirmed complete: API, structure, data and UI, with a separate signed go-live.
   **We are not abandoning the old MJCC.** Its data, its AI stack and its working rules carry forward. Once staging is accepted, the new MJCC is merged into production and inherits everything. What changes is the home: `KpnSolute/Compute` stops being where MJCC lives; `KpnWorld/MJCC-workspace` (template, plugin, UI) on CEngine becomes the home. Design the migration (W1) so it can be re-run as the final cutover merge, not as a one-off copy.
2. **Staging is `aqkhebljkagytsqtkkly`** ("CE-Engine MJCC", Testing org, schema `ce_mjcc_staging`). Full read and write there. `qprfonxvthmaoxfixigk` (MJCCv2) holds LunchVoice survey/menu data (dish_ratings, survey_answers, sessions); read it for the menu-feedback integration, do not write.
3. **Synthetic or sanitized fixtures only** in repositories. Real MJCC rows may be read for migration mapping and reconciliation totals, never committed.
4. **No old FastAPI behind the new API.** Every business decision executes in CEngine or an approved plugin.
5. **Production grade, not a demo.** Every endpoint: tenant-scoped, role-checked, idempotent on POST, audited, covered by an HTTP + Postgres test. Decimal money, never float. Concurrency proven with two writers.
6. **Backend before UI.** Thursday's UI team consumes `ui/API_CONTRACT_DRAFT.md`; by Wednesday night that file must be the *final* contract, generated from the running server's OpenAPI (`ce-server/src/openapi.rs`), not hand-written.
7. Small PRs per capability with tests. Documentation alone is not a milestone.

---

## 3. Where we are (evidence, read 2026-10-19)

### Proven and keep
| Capability | Engine | Template / plugin | Staging table | Proof |
|---|---|---|---|---|
| Three-week valuation, carried opening cost | `ce-plugin/modules/inventory.json` | `stock_item`, `stock_line`, `stock_movement` | yes | `legacy_parity.rs` (164 rows), `golden/week1`, `golden/rollover_week2` |
| Over-pull rule | plugin slots `ending_qty`, `over_pull` | invariant `no_over_pull` | yes | `plugins.rs::an_existing_shortfall_may_shrink_but_never_grow…`, `golden/over_pull` |
| 25 % price decision, half-cent tolerance | module slot `price_decision` | `price_observation` + 3 actions | yes | `legacy_parity.rs` (326 pairs), `golden/price_observations` |
| Period / week lifecycle, lock, publish, close | `ce-server/src/periods.rs` | `period`, `period_week`, 4 actions, invariant `three_operational_weeks` | yes | `tests/ops_smoke.py`, `tests/backend_proof.py` |
| Changesets (Source Control) | `ce-server/src/changesets.rs` stage/merge/revert/preview | `changeset`, `changeset_op` | yes | backend_proof |
| Outbox events, relay, dead-letter | `ce-events`, `ce-server/src/events.rs`, `deliveries.rs` | `outbox_event` | yes | backend_proof |
| Declarative reports | `ce-server/src/reports.rs` | 8 reports | n/a | backend_proof |
| Roles, tenant scope, token verify, PIN login host | `ce-auth`, `ce-server/src/login.rs` (lockout, min PIN, forced rotate) | 5 roles | `_ce_staff`, `_ce_staff_event` | backend_proof signs in 4 roles |
| HACCP limits/logs, menus, meal periods, snack bar (computed tax/discount/total), incidents, checklist, daily log, events, budgets, vendors, ServSafe | template only | 35 entities | 46 tables | backend_proof covers HACCP, menu, snack bar |
| Barcodes | `ce-barcode`: QR, Code128, EAN, SVG | `item_barcode` | yes | unit tests only, no endpoint |

### Live system facts that matter
- MJCCv1 `public` has **88 tables**; the template plus module give **46**. The 42 missing are mostly platform (workspaces, provisioning, AI providers, SSO, GitHub sync) or backups (`july_reimport_*`). The operational ones we still owe are listed in §5.
- `audit_events` has 667 K rows, **655 K of them are `http.request`**. Business actions are a few hundred (`staging.submit:inventory_week_update` 162, `commit.approve` 136). Lesson: keep access logs out of the business audit table. CEngine's `audit_event` already does this; keep it that way.
- `haccp_logs`, `incident_logs`, `inventory_audit_log` have **0 rows** in production. Those features were built and never adopted. The new versions must be reachable in two taps on a tablet or they will die the same way (UI team note, but the API must support "today's checks for my location" in one call).
- Real volumes are small: 443 items, 2.1 K monthly lines, 6.4 K movements, 1 K invoice lines, 27 archived files. Correctness and workflow matter more than throughput.

### Where it went wrong (fix, do not repeat)
1. **Scope drifted to inventory.** The blueprint says "not a QR inventory demo", yet all golden tests, the only module and the only plugin are inventory. HACCP, menus, snack bar, checklists exist only as entity definitions with no rules, no reports proven, no fixtures.
2. **Documents claimed more than code.** `CEngine/README.md` still says store, events and server are "not done"; `spec/capability-map.md` is full of "unverified". Update docs the same PR that changes the code, and delete stale claims.
3. **Rollover client was never built.** `CONTRACT_PARITY.md` admits it. Without it the three-week cycle cannot turn a month.
4. **Files are in memory.** `ce-server/src/files.rs::MemoryBlobs` is the only blob store; `ce-store-pg/src/blobs.rs` is a stub. The template declares `storage.files: true`. Invoices, File Vault and avatars have nowhere to live.
5. **No scheduler, no notifications, no document intake, no report output, no search, no PII enforcement** in the engine (grep counts: `schedul` 1 file, `webhook` 0, `sms` 0, `ocr` 0, `backup` 0; `pii: true` flags in the template are never read by engine code).
6. **QR was shipped as a library, not a feature.** `ce-barcode` encodes; nothing serves a label or resolves a scan.
7. **No CI in MJCC-workspace.** Goldens and proofs run by hand.
8. **Manager role lists actions (`rollover_stock`, `settle/apply/keep_price_observation`) that the template's `actions` block does not define.** They come from the module; confirm they resolve at manifest-compose time and fail loudly if not.

---

## 4. Target architecture for Wednesday

```
                         ┌────────────────────────────────────────────────┐
  UI (Thu+) ───HTTPS────▶│ ce-serve  (one binary, CE_* env)               │
  MCP / scripts          │  ce-server: auth · tenant · idempotency · audit │
                         │  entities · actions · changesets · periods      │
                         │  reports · projections · files · scan · search  │
                         │  scheduler · notifications · intake              │
                         └──────┬──────────────┬──────────────┬───────────┘
                                │              │              │
                     modules: workspace.core  foodservice   business.inventory
                                │              │              │
                     plugin: mjcc-inventory 0.4 (rules, valuation, approvals, HACCP limits)
                                │
                 ┌──────────────┴───────────────┐
                 │ Postgres ce_mjcc_staging      │  Supabase Storage (S3 protocol), bucket mjcc-archive
                 │ outbox → relay → connectors   │  MJCC AI stack (ported from Compute) via ce-provider-http
                 └───────────────────────────────┘
```

Data flows that must exist end to end:

**Intake pipeline (documents → ledger):** `POST /files` (blob store, sha256, dedupe) → `import_batch` created → deterministic parser (native PDF text first, as `invoice_parser.py::_extract_text_native` does today) → if confidence < threshold, the **MJCC AI stack** (§4a) via `ce-provider` → extracted lines written as a **changeset** (`supplier_invoice` + `supplier_invoice_line` + `stock_movement` ops) with provenance `{source, parser, confidence, page, bbox}` → unknown SKU → `sku_alias` proposal; price jump → `price_observation` → manager previews diff → `merge_changeset` posts everything in one transaction → outbox emits `invoice.posted` → notification. AI never writes the ledger. Parser offline ⇒ manual line entry still stages the same changeset.

**Scan pipeline (QR → action):** `GET /scan/{code}` resolves opaque code → `{kind: item|location|equipment|staff, record, on_hand, last_movements, allowed_actions}`; `POST /scan/{code}/move` performs receive/pull/count with idempotency key; `GET /labels/{entity}/{id}.svg|pdf` prints. Codes are opaque ULIDs, never SKUs or counts. Unknown, duplicate, unauthorized and simultaneous scans fail safely (test all four).

**Period turn:** scheduler job `period.rollover` (or manager action `rollover_stock`) copies ending → opening with carried cost, idempotent, audited, blocked while any `price_observation` is `pending` for that period.

**Alerts:** HACCP `in_range == false` → `haccp.out_of_range` event → notification to manager + corrective-action task created. Certification `expiry_date - 30d` → task + notification. Scheduler drives daily checklist instantiation.

### 4a. AI across the whole workspace (old MJCC stack, no Infajn)

There is **no Infajn integration in this template**. Port what MJCC already runs, and widen it from "invoice parser + chat" to an assistant that can work on every part of the workspace within the user's permissions. Evidence from MJCCv1 (read 2026-10-19):

- `ai_stack_config` (1 row): default provider **Google, `gemini-2.5-flash`, vision on**.
- `ai_provider_keys` (7 rows): active keys for **google** (default), **google_cloud_vision** (default OCR), **anthropic**, **groq**. Ollama and LM Studio listed in `ai_providers` but unused.
- `ai_usage_logs` (369 rows), by operation:
  | operation | provider / model | calls | success |
  |---|---|---|---|
  | `invoice_vision` | google gemini-2.5-flash | 286 | 83 % |
  | `agent_chat` | google gemini-2.5-flash | 30 | 93 % |
  | `agent_chat` | groq llama-3.3-70b | 19 | 89 % |
  | `invoice_vision_recap_retry` | google gemini-2.5-flash | 11 | 100 % |
  | `invoice_vision` | anthropic sonnet-4-6 / haiku-4-5 | 7 | 100 % |
  | `inventory_save` | mixed | 18 | mostly failing |
- Code to port (logic, not FastAPI): `backend/ai/invoice_parser.py` — native PDF text → `_parse_text_pages` → `reconcile_and_adjust` → fall back to `_google_cloud_vision_images` / `_ocr_space` / local OCR → `extract_invoice_vision` (Gemini, page by page) → `_extract_recap_totals` → `invoice_items_to_ops`. Plus `ai/mapper.py`, `ai/diff.py`, `ai/engine.py`, `ai/tools.py`, `ai/context.py` for the agent.

The old agent is already system-wide in intent: `backend/ai/tools.py` hand-writes 17 tools: reads (`get_dashboard_stats`, `get_inventory`, `get_events`, `get_menu`, `get_reorders`, `get_period_status`, `get_users`, `get_haccp_logs`, `get_daily_logs`, `get_ai_usage`, `get_source_control_status`) and writes (`create_event`, `stage_inventory_save`, `stage_inventory_week_update`, the last two wrapped in a PR by `_wrap_in_pr`). `routes/agent.py` adds threads, history, config and **automations** (`GET/PUT /automations`). It gates each tool by a role check (`_role_ok`). The new system keeps all of that and removes the hand-writing.

#### AI is a first-class user of the workspace, not a parser

AI must be able to **fetch, query, summarise, create, update, approve-request and automate** across every entity in the template (inventory, invoices, HACCP, menus, tasks, shifts, certifications, equipment, budgets, snack bar, documents, calendar), and across future department packs without new code. Design rules:

1. **Tools are generated from the manifest, not hand-written.** CEngine exposes `GET /v1/{inst}/_ai/tools`, built from the composed manifest **filtered by the caller's role**: for each readable entity `list_<e>` (filter, sort, select, cursor via `ce-query`), `get_<e>`, `search` (T3), for each report `report_<r>` (params), for each permitted action `act_<a>`, and for each writable entity `propose_create_<e>` / `propose_update_<e>` / `propose_delete_<e>`. Tool JSON Schemas come from field definitions (type, enum, max_length, required, decimal precision), so a new department pack gets AI tools on install. The 17 old tools map onto generated ones; `get_dashboard_stats` and `get_reorders` become reports.
2. **AI acts as the user, never above them.** The agent calls CEngine with a **delegated token**: same tenant, same roles as the signed-in person, plus claim `actor_kind: "ai"` and `on_behalf_of`. CEngine's existing role checks, `locked_when`, invariants and PII masking (M4) apply unchanged. A staff member's AI cannot do what that staff member cannot do.
3. **Reads are direct; writes are proposals by default.** `list/get/search/report` run immediately. `propose_*` stages a **changeset** (the same Source Control the invoice flow uses) and returns a preview diff; the human confirms in the UI or chat (`merge_changeset` stays a human action for managers). A manifest `ai:` block lets the template mark specific low-risk writes as **direct with confirmation** (e.g. `haccp_log` create, `opening_checklist_entry` done, `task` status), still audited, still idempotent. Default for anything financial (stock, invoice, price, budget, period) is changeset-only. `act_close_period`, `act_merge_changeset`, `act_rollover_stock` are **never** callable by AI tokens; enforce in the engine, not the prompt.
4. **Provenance on every AI write.** `audit_event.provenance = {actor_kind: "ai", model, provider, thread_id, tool, prompt_hash}`; changesets created by AI carry `submitted_by = "<user> via AI"`. Reports can filter AI-originated changes.
5. **Automations are scheduled AI or rule jobs** on the M2 scheduler: manifest `automations:` (trigger = cron | event, e.g. `haccp.out_of_range`, `invoice.posted`, `period.week_locked`; steps = tools to call; output = notification, task, or changeset). Examples to ship: morning brief per role (today's tasks, open HACCP exceptions, deliveries due, certs expiring), weekly reorder suggestion as a draft `purchase_order` changeset, month-end variance summary before `close_period`, price-spike explanation attached to a held `price_observation`. Port the existing `/agent/automations` settings into this.
6. **Context, not dumps.** The agent gets tool access, not table exports. Retrieval for documents (invoices, SOPs, menus) indexes `document` text extracted at intake; embeddings optional later (pgvector on the same Postgres), tenant-scoped, PII-excluded.
7. **Every AI call is metered** in `ai_usage_log` (operation, model, tokens, cost, duration, success, user). Per-tenant and per-role monthly budget in `app_setting`; over budget ⇒ AI degrades to read-only, never blocks manual work.
8. **Model-agnostic.** The provider registry (google default, google_cloud_vision for OCR, anthropic, groq; ollama/lm_studio optional) lives in the AI service; the stack config entity picks per operation (`invoice_vision`, `agent_chat`, `automation`, `summarise`). Tool-calling uses each provider's native function calling, replacing the old `<tool_call>` text protocol in `tools.py:668`.
9. **Same tools over MCP.** CEngine serves the generated tool list on an MCP endpoint (`/v1/{inst}/_mcp`, Streamable HTTP, bearer = user token), so Claude, Codex or any MCP client gets the exact same role-scoped tools as MyAI. One tool surface, many clients.

#### How it lands

1. **CEngine `ce-server/src/ai.rs` grows from `_ai/entry` (drafts) to the full surface:** `_ai/tools` (generated catalog), `_ai/call` (execute one tool as the delegated principal, enforce rules 2–4), `_mcp` (rule 9), delegated-token issue in `ce-auth` (`sign_token` with `actor_kind`, `on_behalf_of`, short TTL). Note: the module doc says "CEngine never calls a model… asks the host's Provider (Infajn)". Keep "CEngine never calls a model"; replace "Infajn" with "the template's AI service" in docs and comments, since this template has no Infajn.
2. **MJCC-workspace `services/ai/`** (Python, stateless, no DB access): the model loop (chat, tool calling, automations runner) and the ported invoice extraction. It holds provider keys; it talks to CEngine only through `_ai/tools` + `_ai/call` with the user's delegated token. Reached by CEngine via `ce-provider-http` (`CE_PROVIDER_URL`, `CE_PROVIDER_TOKEN`, `CE_PROVIDER_TIMEOUT_SECS`) for intake, and directly by the UI for chat (streamed).
3. **Invoice extraction** stays deterministic-first (`_extract_text_native` → `_parse_text_pages` → `reconcile_and_adjust`), then OCR, then Gemini vision with the recap retry, and `invoice_items_to_ops` becomes lines → changeset ops on `supplier_invoice`, `supplier_invoice_line`, `stock_movement`, `sku_alias`, `price_observation`.
4. **Template entities:** `ai_provider`, `ai_stack_config` (per-operation model choice), `ai_usage_log`, `ai_thread`, `ai_message` (user, role, content, tool calls, tool results, tokens), `ai_automation` (name, trigger, steps, enabled, owner, last_run, last_result). Manager/admin write; users read their own threads only (row-level by `owner`).
5. **Secrets stay in the tenant's database (§4c), encrypted.** The existing keys are carried over, not rotated (owner decision 2026-10-19; an offline copy is held by the owner). They move from the plaintext column `ai_provider_keys.api_key` into **Supabase Vault in the MJCC tenant project** (`vault.create_secret`, read via `vault.decrypted_secrets` only by the AI service's DB role). The `ai_provider` entity stores `provider`, `label`, `is_active`, `is_default`, `model_override`, `key_ref` (the Vault secret id), never the key. CEngine's control database never sees a tenant key. Staging gets its own test keys in staging Vault.
6. **Fix while porting:** AI `inventory_save` succeeds 2 of 18 times; it is replaced by `propose_update_stock_line` through a changeset. `invoice_vision` at 83 % keeps the retry/recap path, and failures land as `import_batch.status = needs_review`, never dropped.

#### Tests that prove it (add to the daily plan)
- Tool catalog for `staff` contains no `act_close_period` and no PII fields; for `manager` it contains `propose_update_stock_line` but not `act_merge_changeset` when the token is AI-kind.
- Agent asked "log cooler 2 at 44°F" as staff → `haccp_log` created directly (template marks it direct), `in_range=false`, corrective `task` created, notification sent, audit provenance `actor_kind=ai`.
- Agent asked "raise the price of SKU X to $9" as assistant → changeset proposed, nothing posted; manager merges; `price_observation` rule fires.
- Agent with an expired or cross-tenant delegated token → 401/404, nothing staged.
- Automation `morning_brief` runs once per day per role on two `ce-serve` replicas (no duplicates).
- The same `list_stock_item` call through `_mcp` and through `_ai/call` returns identical data.

### 4b. Storage: Supabase Storage, S3 protocol

Supabase provides this; nothing extra to host. Every project has Storage with an **S3-compatible endpoint** (`https://<ref>.storage.supabase.co/storage/v1/s3`, path-style, sigv4) plus per-project S3 access keys generated in Dashboard → Storage → S3 Connection. Buckets can be private with RLS and signed URLs.

Facts from the databases:
- MJCCv1 `archived_files` (45 live rows: **42 invoices, 3 documents, ~54 MB**) all reference bucket `mjcc-archive`, but that bucket is **not** in MJCCv1's Supabase Storage (only `profile-avatars` and `kpncompute-artifacts` exist, 0 objects). The files live in a separate S3-compatible "home archive" configured by `MJCC_ARCHIVE_ENDPOINT/ACCESS_KEY/SECRET_KEY/BUCKET` (`backend/archive_storage.py`, boto3, path-style). That is why commit #21 had to keep uploads working "when the home archive is unavailable".
- Staging (`aqkhebljkagytsqtkkly`) has **no buckets yet**.

Plan:
1. Create private bucket `mjcc-archive` on staging (file size limit 50 MB to match `MJCC_ARCHIVE_MAX_FILE_MB`, MIME allowlist pdf/png/jpeg/webp/heic/xlsx/csv) and `staff-avatars` (2 MB, images). Generate S3 keys in the dashboard; owner supplies them as `CE_BLOB_*` env.
2. `ce-store-pg/src/blobs.rs` implements the **same contract `archive_storage.py` uses**: object key `category/YYYY/MM/<sha256>-<safe_filename>`, sha256 dedupe, categories `invoice|menu|recipe|report|document|other`. Same layout means the migration is a straight copy.
3. W1 migration copies the 45 objects from the home archive into Supabase `mjcc-archive`, verifies sha256 against `archived_files.sha256`, and creates `document` records linked to the migrated invoices.
4. Production gets its own bucket in the production project at cutover; staging and production never share a bucket.
5. Plan limits to check with the owner: Free plan 1 GB storage / 50 MB per file; Pro 100 GB included and larger per-file limits. Current volume (~54 MB) fits either.

### 4c. Two kinds of database: CEngine control plane vs tenant (consumer) data

CEngine is a platform that serves many tenants, so **CEngine's database is not the customer's database**.

| | **CEngine control DB** (KpnSolute owns; Supabase org "Testing") | **Tenant DB** (one per consumer; MJCC's is a JobCorp Supabase project) |
|---|---|---|
| Holds | installations (tenant id → template, version, tenant DB location), plugin registry (slug, version, sha256 digest, approval, active/rollback log — today `installs.json`), manifest hashes, signing-key ids and token revocation, platform usage metering, platform operators | every business record of the template (all entities, `audit_event`, `outbox_event`, changesets, `_ce_staff`, `_ce_idempotency`, `_ce_job`, `_ce_notification`), the tenant's blob bucket, the tenant's **secrets in Supabase Vault** (AI provider keys, SMTP, any connector credentials) |
| Never holds | tenant business rows, tenant files, tenant secrets | other tenants' anything, engine signing keys |
| Who writes | engine operators, `ce-serve plugin approve/activate` | `ce-serve` acting for that installation only |

Rules:
1. **Config split in `ce-serve`:** add `CE_CONTROL_DATABASE_URL` (+ `CE_CONTROL_SCHEMA`, default `ce_control`). `CE_DATABASE_URL`/`CE_SCHEMA` keep meaning *the tenant DB* for the installation. The install record moves from the `CE_INSTALLS` JSON file into control-DB tables `installation`, `plugin_release`, `plugin_install` (same fields as `installs.json`: slug, version, digest, active index, disabled, log). Keep the file path as a bootstrap/import path so the current `run-staging-test.sh` still works.
2. **Tenant DB location is a reference, not a secret in the control DB.** `installation.tenant_db_ref` names a secret (env var name or a secret-manager key) holding the tenant connection string; the control DB stores the reference only.
3. **Tenant secrets resolve at the tenant.** The AI service and connectors read keys from the tenant project's Vault using a dedicated tenant DB role with `select` on `vault.decrypted_secrets` and nothing else. CEngine passes `key_ref`s, never key values, and never logs them.
4. **The Supabase org "Testing" (`dqztbguopqwbmhnmdsle`) is CEngine's home** (owner decision 2026-10-19). The CEngine control DB lives there, in its **own project**, separate from any tenant project: create "CEngine Control" in the Testing org (schema `ce_control`). Do not put `ce_control` inside a tenant project, and do not repurpose the Recon project (`luielvfdljkuxjezqnjb`) without the owner's say-so. Creating a project may add cost: ask the owner before calling the create-project API.
5. **Tenant projects belong to the tenant's org.** Today the MJCC staging tenant DB (`aqkhebljkagytsqtkkly`, schema `ce_mjcc_staging`) also sits in the Testing org. That is fine for this week's build. At cutover the MJCC tenant DB is in the **JobCorp org** (`qggdncgjvnmedihxqpww`): either MJCCv1 `mgvyylvmkxhhataavqjz` receives the new schema via the W1 migration/merge, or a fresh JobCorp project (owner decides before cutover). JobCorp's data, files and Vault keys never leave JobCorp's org; CEngine control in the Testing org reaches them only through `installation.tenant_db_ref`.
6. **Tests:** a second tenant installation on staging (synthetic "acme" template) proves the control DB routes each token to the right tenant DB and that one tenant cannot read another's rows, files or secrets.

Add to Monday as **M6 `control plane: CE_CONTROL_DATABASE_URL + installation/plugin tables`**, doc `spec/plugins.md` §"Installing and running a plugin" and new `spec/control-plane.md`.

---

## 5. Work breakdown by day

Each item names the repo, the PR title, the test that proves it, and the doc to update. Finish in order inside a day; parallelize across the two repos with subagents where marked ⇄.

### Monday 2026-10-19 (rest of today): unblock the engine

**CEngine**
- **M1 `storage: Supabase/S3 blob store`** — implement `ce-store-pg/src/blobs.rs` against Supabase Storage's S3 endpoint (bucket `mjcc-archive` on staging, §4b), object-key layout identical to `archive_storage.py::build_object_key`, sha256 dedupe, tenant prefix, size cap from manifest `max_file_mb`, signed download URLs. Env: `CE_BLOB_URL`, `CE_BLOB_BUCKET`, `CE_BLOB_KEY`, `CE_BLOB_SECRET`, `CE_BLOB_REGION`. Create the bucket with `supabase_execute_sql` on staging only (`insert into storage.buckets …`) or the dashboard. Test: upload, dedupe on same bytes, download, delete, cross-tenant 404. Doc: `spec/README.md` run section; remove "Not done yet" lines that are now false.
- **M2 `scheduler: ce-jobs crate`** — table `_ce_job` (tenant, name, cron or `run_at`, payload, lease, attempts, last_error), leased by `ce-serve` loop (`CE_JOBS=1`), manifest `jobs:` block (`{name, schedule, action|event}`). Test: two serve processes, one DB, each job runs once; failed job retries with backoff; dead-letter after N. Doc: `spec/plugins.md` → new `spec/jobs.md`.
- **M3 `scan + labels endpoints`** — `GET /v1/{inst}/scan/{code}`, `POST …/scan/{code}/move`, `GET …/labels/{entity}/{id}.svg`. Generic: any entity with a field `type: "code"` (new field type, auto-ULID) is scannable. Test: the four failure modes + a happy receive and pull with idempotency replay. Doc: `spec/manifest.schema.json` (field type `code`), `spec/README.md`.
- **M4 `pii: enforce field flags`** — engine reads `pii: true`: strip from public projections, mask in `audit_event.before/after` unless role has `pii_read`, exclude from search index. Test: projection leak test, audit mask test. Doc: `spec/README.md` security section.
- ⇄ **MJCC-workspace M5 `ci: goldens + proof on every PR`** — `.github/workflows/ci.yml`: pin CEngine image digest, Postgres service, run `ops_smoke.py`, `backend_proof.py`, golden comparison. Doc: `README.md`.

**End-of-day check (Monday):** `cargo test --workspace` green; staging has a bucket with one uploaded fixture PDF; `GET /scan/{code}` answers for a staged `stock_item`; CI badge on MJCC-workspace.

### Tuesday 2026-10-20: the two pipelines and the core pack

**CEngine**
- **T1 `intake: ce-intake crate + /intake endpoints`** — `POST /intake/{kind}` (kind from manifest `intake:` block) takes a file id, sends it to the MJCC provider service (§4a) through `ce-provider-http` (timeout, usage written to `ai_usage_log`), and emits a **changeset** with provenance per op.
- **T1a `ai: generated tools, delegated tokens, _ai/call, _mcp`** — §4a "How it lands" item 1. Tool catalog generated per role from the composed manifest; delegated AI token (`actor_kind=ai`, `on_behalf_of`, TTL ≤ 15 min); `_ai/call` enforces read-direct / write-as-changeset / `ai:` direct-allowlist / never-callable actions; provenance on audit; `_mcp` endpoint serving the same tools. Tests: the six AI tests in §4a. Doc: `spec/ai.md` (new); fix the "Infajn" wording in `ai.rs` and `ce-tools/src/lib.rs` doc comments.
- ⇄ **MJCC-workspace T1b `services/ai: port the Compute AI stack`** — move `backend/ai/*` logic into `services/ai/` (FastAPI or Starlette, stateless, no DB): `POST /extract/invoice` (native text → OCR fallback → Gemini vision → recap totals → lines + provenance), `POST /chat` (streamed; native function calling over `_ai/tools` + `_ai/call` with the user's delegated token), `POST /automations/run/{name}` (called by the scheduler). Provider registry and per-operation model choice from `ai_stack_config`. Template adds `ai_provider`, `ai_stack_config`, `ai_usage_log`, `ai_thread`, `ai_message`, `ai_automation` and an `ai:` block listing direct-write entities (`haccp_log`, `opening_checklist_entry`, `task.status`, `meal_count`, `waste_log`). Tests: the 42 migrated invoice PDFs are not fixtures (real data); build 5 synthetic invoice PDFs per vendor layout and golden their extracted lines. Never writes entities directly. Test: fixture PDF → changeset with 12 ops; parser offline → `import_batch.status = needs_manual`, nothing posted; same file twice → one batch (sha256). Doc: `spec/intake.md` (new), diagram B from the blueprint moved here.
- **T2 `notify: connector + templates`** — outbox subscriber `notify` with channels `inapp` (table `_ce_notification`, `GET /notifications`, `POST /notifications/read`), `email` (SMTP or Resend via env), `webhook`. Manifest `notifications:` maps event → recipients (role or field) → template. Test: HACCP out-of-range creates in-app + email record; mark-read; retry on 5xx. Doc: `spec/notifications.md`.
- **T3 `search: /search?q=`** — Postgres `tsvector` over fields marked `searchable`, tenant-scoped, role-filtered, PII-excluded. Test: finds item by SKU and description, not by PII field. Doc: `spec/README.md`.
- **T4 `reports: csv + pdf output`** — `GET /reports/{name}.csv|.pdf?params`. PDF via a pure-Rust renderer (e.g. `printpdf`/`genpdf`), one portrait template with title, params, table, totals, footer with manifest hash and generated-at. Test: `stock_valuation_by_period` CSV totals equal JSON report; PDF non-empty and contains title. Doc: `spec/README.md`.

⇄ **MJCC-workspace**
- **T5 `template: workspace.core layer`** — split `mjcc.template.json` into `template/core/*.json` + `template/foodservice/*.json` composed by a build step (or manifest `include`, add to engine if cheap). Add core entities: `task` (title, assignee→staff_member, due_at, status open/in_progress/done/cancelled, source_entity, source_id, priority), `shift` (staff_member, starts_at, ends_at, station), `calendar_event` (generalises `cafeteria_event`; keep the old name as a view or alias), `equipment` (name, location, code, last_service_at, next_service_at), `equipment_log`, `certification` (generalises `servsafe_certification`: staff_member, kind, issued, expires, proctor, file), `training_record`, `location` (storage/kitchen/dining; code), `notification` is engine-owned. Roles: add `supervisor` between assistant and manager; add scopes by `location`. Tests: ops_smoke extended; role matrix test asserting each role's read/write per entity. Doc: `docs/CONTRACT_PARITY.md` + new `docs/TEMPLATE_LAYERS.md`.
- **T6 `template: foodservice completeness`** — add `receiving_log` (invoice line or PO line, temp, condition, accepted, by, at, photo file), `storage_location` on `stock_item_detail` (ref location), `lot` (item, lot_no, expires_on, received via) and `lot` ref on `stock_movement`, `meal_count` (business_date, meal_period, program, planned, served, leftover), `waste_log` (item or menu_item, qty, unit, reason enum, by, at), `substitution_log`, `purchase_order` + `purchase_order_line` (status draft/sent/received/closed; reorder suggestion computed from par − on_hand − on_order), `allergens` (json) on `menu_item`, `recipe` + `recipe_line` (later: costing computed from current price). Give every item, location, equipment and staff badge a `code` field so QR works everywhere. Tests: goldens for `reorder_suggestion`, `meal_count` report, `waste_by_reason` report. Doc: `docs/CONTRACT_PARITY.md`.
- **T7 `plugin: mjcc-inventory 0.4`** — add `rollover_stock` action (idempotent; refuses while `price_observation.pending` > 0 for the period; emits `period.rolled`), HACCP corrective-action rule (out of range ⇒ `task` created, `corrective_action` required before `haccp_log` can be edited again), receiving acceptance rule (temp outside limit ⇒ `accepted=false` forces a note). Tests: `golden/rollover_month_turn.expected.json`, `golden/haccp_corrective.expected.json`.

**End-of-day check (Tuesday):** `backend_proof.py` extended to: upload invoice PDF → intake → changeset preview shows lines → manager merges → stock and price observation updated → month rolled over → report PDF downloads → notification delivered. Plus an AI pass: staff chat logs a HACCP reading directly; assistant chat proposes a price change that only a manager can merge; manager chat answers "what's below par this week" from a report tool; `morning_brief` automation produces one notification per role. Must pass on staging.

### Wednesday 2026-10-21: migration, hardening, contract freeze

**MJCC-workspace**
- **W1 `migration: v1 → CEngine ETL`** — `tools/migrate_v1.py` reads MJCCv1 **read-only** (pooler session mode or MCP `supabase_query`) and writes staging via the CEngine API (not SQL) so every row is validated and audited. Mapping table in `docs/MIGRATION_MAP.md`: `inventory_items→stock_item+stock_item_detail`, `monthly_inventory→stock_line` (w1..w3 received/pulled → `stock_movement` rows), `inventory_transactions→stock_movement`, `invoices/invoice_items→supplier_invoice(_line)`, `menu_*`, `events→calendar_event`, `user_profiles→staff_member` (no PINs copied; staff re-enrol), `archived_files→document` (45 objects, ~54 MB, copied from the home archive into Supabase `mjcc-archive`, sha256-verified), `budget_*`, `vendors`, `servsafe_certifications→certification`, `flow_assignments→task`, `ai_stack_config/ai_providers→ai_stack_config/ai_provider` (no key values), `ai_usage_logs→ai_usage_log`, `agent_conversations/agent_threads` → provider-service store. The script must be **re-runnable** (upsert on legacy id kept in `provenance.legacy_id`) because the same script performs the final merge at cutover. Reconciliation script prints per-period opening/received/pulled/ending totals old vs new to the cent; differences must be explained in the doc (legacy float rounding cases are known). Run against a **sanitized** copy for CI; run against real v1 read-only once, keep only totals in the report.
- **W2 `multiuser proof`** — two concurrent pulls on one low-stock line, two concurrent intake merges touching the same item, idempotency replay under load. Record results in `docs/RELIABILITY.md`.
- **W3 `contract freeze`** — generate `ui/API_CONTRACT.md` from the live OpenAPI; per-screen call map updated in `ui/SCREEN_ACTION_MATRIX.md` (every S00–S27 row gets a CEngine endpoint or an explicit "dropped: reason"). Add the new screens: Receiving, Tasks, Calendar/Shifts, Equipment, Certifications, Purchase Orders, Meal Counts/Waste, Scan.
- **W4 `readiness board`** — fill the matrix from blueprint §11 in `spec/mjcc-go-live-assessment.md` with real evidence links (PR, test name, CI run URL, staging request id). Every row must say "staging tested: yes" or carry a blocker.

**CEngine**
- **W5 `ops: backup/restore + release`** — `scripts/backup.sh` (pg_dump schema + bucket sync), `scripts/restore.sh`, tested restore into a fresh schema with the proof passing afterwards. Tag `v0.3.0`, pin digest in MJCC-workspace CI. Update `README.md` to describe reality. Doc: `docs/operations.md` (new).
- **W6 `docs sweep`** — `spec/capability-map.md`: replace every "unverified" with a verdict or delete the row; `spec/README.md` layout lists the new crates; CHANGELOG entries.

**End-of-day check (Wednesday):** staging contains migrated, reconciled MJCC history; `backend_proof.py` and the migration reconciliation both pass in CI; `ui/API_CONTRACT.md` is generated and committed; readiness board has no empty cells. Post the summary with links in both repos' `docs/`.

---

## 6. Definition of done per endpoint (checklist to paste into every PR)

- [ ] Tenant predicate first in every query (ce-query guarantees it; add a test anyway for new tables)
- [ ] Role check from manifest; `supervisor` and `location` scope respected
- [ ] `Idempotency-Key` required on POST/PUT; replay returns the original result
- [ ] `audit_event` row with before/after, PII masked, provenance for intake and plugin version/digest
- [ ] Decimal fields `precision/scale`; no `f64` in money paths
- [ ] Error is a Problem JSON with a code from `spec/catalogs/errors.json`
- [ ] HTTP + Postgres test in `crates/ce-server/tests/` or `tests/backend_proof.py`
- [ ] Doc updated in the same PR; stale sentence removed

---

## 7. Reference points (use for shape, do not copy code)

- **Frappe Framework / ERPNext**: DocType metadata → schema, REST, role permissions, workflow states, per-document user permissions. CEngine's manifest plays the DocType role; `locked_when`, `state`, `roles` are our equivalents. Their "User Permission" (row-level by linked field) is what our `location` scope should look like.
- **Paperless-ngx + invoice2data + docTR/PaddleOCR**: deterministic vendor templates first, OCR second, human review before anything is final, provenance kept per field. Our intake pipeline follows exactly that order.
- **Twenty / Directus / Budibase**: metadata-driven admin UIs; proof that a generic engine plus a thin typed client is a viable product shape for Thursday's UI team.
- **FDA HACCP seven principles**: critical limits, monitoring, corrective actions, verification, records. Our `haccp_limit` + `haccp_log` + corrective `task` + reports cover monitoring, corrective action and records; limits are supplied by MJCC staff, never invented by us.

---

## 8. Tools you have

- `supabase` MCP (full control over every project the PAT sees): `supabase_list_projects`, `supabase_list_tables`, `supabase_describe_table`, `supabase_query` (read-only, safe on production), `supabase_execute_sql` (writes; **staging only**).
- `run-staging-test.sh` in `/home/ogdev/CEngine/new-testing-db/` starts `ce-serve` on :8099 against staging and runs the smoke test.
- Repos: `/home/ogdev/CEngine/new-testing-db/CEngine`, `/home/ogdev/CEngine/new-testing-db/MJCC-workspace`, legacy read-only `/home/ogdev/CEngine/old-mjcc-db/compute`.
- Previous assessment with table-by-table evidence: `spec/mjcc-go-live-assessment.md` (this repo).

Report progress at the end of each day in `docs/DAILY_LOG.md` (MJCC-workspace): what merged, test names, CI links, what slipped and why. If something will not make Wednesday, say so Tuesday night, not Wednesday night.
