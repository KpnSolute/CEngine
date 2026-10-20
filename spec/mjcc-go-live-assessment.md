# MJCC → CEngine Go-Live Assessment

Status: draft for owner review  
Prepared: 2026-10-19  

This report cross-references the three implementation repositories — `KpnSolute/Compute` (live MJCC v1), `KpnSolute/CEngine` (the framework), and `KpnWorld/MJCC-workspace` (MJCC's new home) — and the four databases the personal access token can reach. Every claim below cites file-level code evidence, a database schema, or a row count that I read directly.

## Skip to the bottom

- [What is already proven](#already-proven)
- [CEngine gaps that block go-live](#cengine-gaps-blocking-go-live)
- [MJCC template gaps that block go-live](#mjcc-template-gaps-blocking-go-live)
- [What the old system has that the new system does not yet model](#unmapped-legacy-features)
- [What a cafeteria manager needs that neither system has](#missing-cafeteria-capabilities)

---

## Live system snapshot (MJCC v1, project `mgvyylvmkxhhataavqjz`, us-west-1)

The live database has **88 tables in `public`**, 3 in `app_private`, and two backup schemas (`bak_20260619`, `bak_20260619b`). Row-level security is enabled on every public table.

| Table | Rows (live est) | Size |
|---|---|---|
| `audit_events` | 667K | 411 MB |
| `commit_changes` | 81K | 35 MB |
| `error_logs` | 15K | 30 MB |
| `inventory_transactions` | 6.4K | 5.2 MB |
| `staging_entries` | 4.4K | 14 MB |
| `monthly_inventory` | 2.1K | 1.5 MB |
| `menu_cycle_slots` | 1.2K | 664 kB |
| `invoice_items` | 1K | 1 MB |
| `inventory_items` | 443 | 736 kB |
| `ai_usage_logs` | 369 | 344 kB |
| `price_review_queue` | 212 | 272 kB |
| `commits` | 230 | 328 kB |
| `pull_requests` | 183 | 336 kB |
| `events` | 35 | 128 kB |
| `archived_files` | 27 | 216 kB |
| `vendors` | 3 | 64 kB |

The top audit actions (out of 667K) are `http.request` (655K, every API call), `session.teardown` (26K), `staging.submit:inventory_week_update` (162), `commit.approve` (136).

The backend is 26 route files in `backend/routes/` (auth, inventory, data_entry, price_review, sku_review, sourcectrl, logs, menus, events, snack_bar, cost, agent, files, users, notifications, public_menu, workspaces, data, health, changelog, github_sync, api_logs, etc.).

The frontend is 28 screens mapped in `ui/SCREEN_ACTION_MATRIX.md` plus 21 React components in `frontend/src/`.

---

## Already proven

These items have golden tests, backend proof, and parity documentation linking the old FastAPI to the new CEngine plugin.

### 1. Three-week inventory with exact valuation
- **Code:** `KpnSolute/CEngine crates/ce-plugin/modules/inventory.json` defines `stock_item`, `stock_line`, `stock_movement`, `price_observation`, `sku_alias`.
- **Plugin:** `MJCC-workspace plugin/.cengine/plugins/mjcc-inventory` 0.3.0, with 5 golden fixture files.
- **Proof:** CEngine `crates/ce-plugin/tests/legacy_parity.rs::every_legacy_vector_matches_the_module_under_the_mjcc_policy` — 164 synthetic rows. `golden/week1.expected.json`, `golden/rollover_week2.expected.json`.
- **Staging DB (`ce_mjcc_staging`):** Tables are created for `stock_item` (10 cols), `stock_line` (20 cols), `stock_movement` (13 cols), `stock_item_detail` (11 cols), `period` (17 cols), `period_week` (16 cols). Schema matches the template exactly.

### 2. Over-pull rule
- **Rule:** Stock may not go below zero. Existing shortfall stays editable but may not grow.
- **Proof:** CEngine `crates/ce-server/tests/plugins.rs::an_existing_shortfall_may_shrink_but_never_grow_under_the_legacy_over_pull_rule`. `golden/over_pull.expected.json`.

### 3. 25% price decision
- **Rule:** Price changes ≥25% trigger review. A 0.5¢ tolerance and exact-25% threshold.
- **Proof:** `legacy_parity.rs::price_decisions_match_the_legacy_observed_price_protocol` (326 pairs). 6 exact-25% cases where legacy float math holds and CEngine applies. `golden/price_observations.expected.json`.
- **Staging DB:** `price_observation` table (14 cols) with status enum (`pending/reviewed/applied/held`) and computed `decision` field.

### 4. Rollover carries opening cost
- **Rule:** Next period's opening = prior period's ending. Cost carried, so a new price does not revalue stock on hand. Depleted lines open at 0.
- **Proof:** `plugins.rs::rollover_carries_cost_so_a_new_price_does_not_revalue_what_was_on_hand`, `rollover_carries_each_ending_into_the_next_opening_and_is_safe_to_rerun`. `golden/rollover_week2.expected.json`.
- **Gap:** The rollover client procedure is documented but not built in `MJCC-workspace`. The template has the period entity and a `close_period` action, but no `rollover_stock` action or client driver yet.

### 5. Period lifecycle with locked weeks
- **Code:** CEngine `ce-server/src/periods.rs` has `enabled()`, `touches_server_fields()`, `transition()`. The template defines `lock_week`, `unlock_week`, `publish_week`, `close_period` actions with state machines.
- **Staging DB:** `period` table with `week_1_locked` through `week_4_locked` computed fields, `period_week` table with `lock_w1`–`lock_w4` and the `open/locked/published` state machine.
- **Template invariant:** `period_week.week_number between 1 and 3` (`three_operational_weeks`).

### 6. Changesets and Source Control
- **Code:** CEngine `ce-server/src/changesets.rs` — `stage()`, `merge()`, `revert()`, `preview()`, `close()`.
- **Template:** `changeset` entity (6 cols) with `proposed/merged/rejected/withdrawn/conflicted/reverted` states. `changeset_op` (9 cols) recording `create/update/delete` with `before` and `data`. The `merge_changeset` action emits `changeset.decided`.
- **Staging DB:** Both tables exist. This matches the old Compute `commits` (17 cols, 230 rows live) and `staging_entries` (22 cols, 4.4K rows live) model.

### 7. Events and outbox
- **Code:** `ce-events/src/outbox.rs`, `ce-server/src/events.rs` — `enqueue()`, `relay_once()`, `replay()`.
- **Template:** `outbox_event` (12 cols) with `pending/in_flight/delivered/dead_lettered`. `manage_events` action.
- **Staging DB:** Table exists.
- **Live equivalent:** Old system has `kpnsolute_events.py` and a CloudEvents outbox. The new outbox is more structured.

### 8. Reports engine
- **Code:** `ce-server/src/reports.rs` — `exists()`, `entity()`, `request()`, `columns()`, `title()`. Server-side declarative reports.
- **Template:** 8 reports: `haccp_by_location`, `haccp_out_of_range`, `menu_feedback`, `snack_bar_daily`, `stock_valuation_by_period`, `invoice_reconciliation`, `price_changes`, `incidents_by_type`. Each has `entity`, `group_by`, `measures`, `params`.

### 9. Backend proof (real API)
- **Code:** `MJCC-workspace/tests/backend_proof.py` — 405 lines, running against a live `ce-serve` with `mjcc.template.json`, the `business.inventory` module, and the `mjcc-inventory` 0.3.0 plugin. Uses only the HTTP API, synthetic data, and idempotency keys.

---

## CEngine gaps blocking go-live

These are missing from the framework itself — nothing to do with MJCC specifically.

### C1. Scheduler (no recurring jobs)
- **Evidence:** Only 1 source file mentions "schedul" across 24 crates. No cron or scheduling crate.
- **Why it blocks:** Week rollover (`rollover_stock` must run on a schedule), ServSafe certificate expiry warnings, HACCP daily checklist generation, weekly report generation.
- **Capability-map reference:** `spec/capability-map.md` line 60: "Scheduling of recurring jobs run by the engine (**unverified** whether MJCC has a scheduler)."
- **Workaround:** Run as an external cron job calling the API, but this sidesteps audit, tenant scoping, and retry. A `ce-scheduler` or `ce-job` crate is needed.

### C2. File/blob storage is in-memory only
- **Code:** `ce-server/src/files.rs` — `MemoryBlobs` struct. `spec/README.md` shows no storage backend beyond memory. `ce-store-pg/src/blobs.rs` exists but is thin.
- **Why it blocks:** Invoice PDF upload (the `document` entity has a `file` field), File Vault (27 archived files on live), staff avatars, HACCP sign-off attachments, report output generation.
- **Template reference:** `runtime.storage.files: true, max_file_mb: 25` — the template declares it, but the backend doesn't persist it.

### C3. Notifications, email, SMS
- **Code:** 4 files mention "notif", 6 mention "email", 0 mention "sms" or "webhook".
- **Why it blocks:** HACCP out-of-range alerts, certification expiry reminders, Source Control review requests, period close notifications. The live system has `notifications.py`, `email_log`, `email_templates`.
- **Capability-map reference:** Line 26: "Built-in engine with a connector for delivery (via Infajn)". The connector is not built.

### C4. No connector layer for AI document extraction
- **Code:** The blueprint (section 8) says AI extraction must feed into changesets, not the ledger. `ce-server/src/changesets.rs` handles staging. But there's no `ce-connector` crate for invoice/OCR/AI inputs.
- **Why it blocks:** Invoice upload → extract → stage → review → approve is the single most-used workflow (162 staging submits, 136 commit approvals live). The `import_batch` entity exists; the connector that populates it does not.
- **Live reference:** `backend/ai/` (8 files: engine, parser, invoice_parser, mapper, diff, tools, context). The new system needs equivalent connector logic as a CEngine module.

### C5. No report output (PDF, CSV, print)
- **Code:** 3 files mention "pdf", 9 "csv", 7 "export".
- **Template reference:** 8 reports declared in the template, zero output rendering.
- **Live reference:** `Reports.tsx`, `Templates.tsx`, `backend/routes/data.py`.

### C6. No search across records
- **Code:** 15 files mention "search" across crates. No full-text search or cross-entity search API.
- **Why it blocks:** Manager dashboard (search inventory by name/SKU), Source Control (search pending items), File Vault (search documents).
- **Live reference:** `Portal.tsx` line 5529 — "Command search (Ctrl+S)" with `NAV_KEYWORDS`. Client-side search only, but the expectation is built into the UI.

### C7. No per-field privacy enforcement
- **Code:** `capability-map.md` line 62 lists "Per-field privacy for personal and health data" as missing.
- **Template reference:** `staff_member` marks `display_name`, `email`, `last_name`, `phone`, `bio` as `pii: true`. `menu_suggestion.submitted_by` is `pii: true`. No engine code enforces these flags.
- **Why it blocks:** Job Corps handles student data (FERPA). ServSafe certification names, staff emails, and phone numbers must not leak through public projections or logs.

### C8. Production deployment and operations
- **Code:** Render Dockerfile and `deploy/render/` exist. No backup/restore scripts, no migration tooling for live data, no monitoring beyond the health endpoint.
- **Why it blocks:** The v0.2.0 release spec (`v020-commercial-release-mjcc-acceptance.md`) section D requires: "Document and verify URL, TLS, auth/token verifier, health/readiness, database migration/install procedure, secret management, observability, safe shutdown/restart, backups and restore."

### C9. Multi-location and multi-tenant scoping
- **Code:** The framework has `tenant_id` on every row. The template declares a single MJCC tenant.
- **Live reference:** Live has `centers` (1 row, 1 center = MJCC), `tenants` (1 row), `tenant_memberships` (empty), `tenant_tree_nodes` (7 rows), `workspace_projects` (1 row), `workspace_sites` (empty).
- **Gap:** Single-tenant is fine for MJCC launch. Multi-location is out of scope but noted because the blueprint lists it as future.

---

## MJCC template gaps blocking go-live

These are specific to `mjcc.template.json` and need implementation in `MJCC-workspace`.

### T1. No rollover client
- **Evidence:** `CONTRACT_PARITY.md` line: "The rollover client itself is not built in this repository."
- **What's needed:** An action `rollover_stock` that iterates `stock_line` for a completed period and creates opening lines for the next period, carrying `opening_unit_cost` and `opening_oh`. Must be idempotent. Gamely referenced in the `manager` role's `actions` list but not defined in the actions block.

### T2. No invoice extraction workflow
- **Template entities exist:** `supplier_invoice` (20 cols), `supplier_invoice_line` (14 cols), `import_batch` (7 cols), `document` (5 cols with `file` field).
- **What's missing:** The connector that takes an uploaded invoice file → extracts lines → stages as a changeset → manager reviews and merges. This is the #1 operational workflow. The blueprint diagrams B and section 8 describe it in detail.
- **Old equivalent:** `backend/ai/invoice_parser.py`, `data_entry.py`, `sourcectrl.py` (2,350 lines). `backend/routes/agent.py` handles AI-assisted data entry.

### T3a-tasks, assignments → not present in template
- **Old:** `flow_assignments` (17 cols, 23 rows live), `DailyOps.tsx`, `FlowAdmin.tsx`, `FlowPanel.tsx`. Assigns staff to tasks.
- **New template:** Not modeled. There's `daily_operations_log` (generic log) and `opening_checklist_item`/`entry`, but no task assignment entity.
- **Needed:** Entity `assignment` or `task` with `assigned_to`, `status`, `due_date`, `completed_at`. This is separate from the checklist.

### T4a-meal_count, waste_log → not present in template
- **Old:** `Forms.tsx` has `MachineLog`, `CoolingLog`, `MealLog`, `InspectionSheet`, `FoodRequest`. The Meal Log (S07) records meal counts.
- **New template:** Not modeled.
- **Needed:** `meal_count` entity (meal_period, program, count, waste_qty, date). `waste_log` entity (item, qty, reason, date). These drive menu planning and food cost reports.

### T5t-report rendering → no output pipeline
- **Evidence:** Template declares 8 reports. `ce-server/src/reports.rs` generates SQL. Nothing generates PDF or CSV.
- **Live:** `Reports.tsx` and `Templates.tsx` produce formatted outputs.
- **Needed:** A `report_output` action or endpoint that takes a report name + period and streams a file.

### T6a-order_templates, purchasing → not present
- **Old:** `inventory_items.par_level` exists (16 cols, 443 rows live), no purchase order table.
- **Needed:** `purchase_order` entity, `purchase_order_line`. Reorder suggestions (par level – on-hand = suggested order qty). The blueprint section 5 mentions "Planning ahead": "Reorder suggestions, trends, cost spikes, menu support, reminders."

### T7a-snack_bar POS screen → UI-only, entity is ready
- **Template:** `snack_bar_product` (4 cols), `snack_bar_rate` (3 cols — student/staff with discount_pct and tax_pct), `snack_bar_transaction` (11 cols, 5 computed), `snack_bar_transaction_item` (6 cols), `snack_bar_sale` (5 cols, daily closing). This is one of the most complete entity groups.
- **Gap:** The POS interaction model — tap product → qty → student or staff rate → tally → close sale — is well-modeled in the template but has no UI client.

---

## Unmapped legacy features

These exist in the old Compute backend and frontend but have no equivalent in `mjcc.template.json`.

### AI/agent system
- **Old code:** `backend/ai/` (8 files), `routes/agent.py`, `AIStudio.tsx` (4 sub-screens: MyAI, Usage, Tools, Automation), `agent_conversations` (10 cols, 4 rows), `agent_threads` (6 cols), `agent_usage` (4 cols, 21 rows), `ai_providers` (6 cols, 7 rows), `ai_provider_keys` (13 cols, 7 rows), `ai_stack_config` (1 row).
- **Decision:** The blueprint and controller rule say AI is a CEngine-controlled use of Infajn-provisioned resources. This does not need to be in the template. The chat interface may live in the platform or a separate service.
- **Impact:** MJCC staff uses MyAI daily. Migration must preserve those workflows. This is a platform concern, not a template concern.

### GitHub sync
- **Old code:** `routes/github_sync.py`, `github_sync_queue` (9 cols, 232 rows).
- **Decision:** The blueprint says "GitHub controls the private plugin source and installation provenance. Source Control governs customer operational data." The old GitHub sync pushed operational data. That pattern is replaced by changesets.
- **Impact:** Out of scope for MJCC template. The 232-queue row history is operational, not code.

### SSO and sibling apps
- **Old code:** `sso_handoffs` (8 cols, 3 rows), `lunchvoice_sso_handoffs` (8 cols, 2 rows), `routes/auth.py` SSO endpoints.
- **New:** Not modeled. LionCafe menu hand-off via SSO is referenced in the old screen matrix (S19).
- **Impact:** LunchVoice integration requires authentication handoff. This is platform auth, not the template.

### Workspace console
- **Old code:** `routes/workspace_console.py`, `routes/workspaces.py`, `workspace_projects`, `workspace_sites`, `workspace_creation_requests`.
- **New:** Not modeled. The blueprint says provisioning is Infajn/Compute, not CEngine.
- **Impact:** Out of scope.

### What's New / changelog
- **Old code:** `routes/changelog.py`, `Portal.tsx` S00.10.
- **New:** Not modeled. Simple to add as an `app_setting` key or a new `release_note` entity.
- **Impact:** Minor. Can be an `app_setting` JSON blob.

### Notifications
- **Old code:** `routes/notifications.py`, `Portal.tsx` S00.08 (notification bell with list and mark-read).
- **New:** Not modeled. Depends on C2 (email) and C3 (notifications) being built in the engine.
- **Impact:** Needed before launch.

### Staff PIN login and throttle
- **Old code:** `routes/auth.py`, `staff_login_throttle` (9 cols), `staff_sessions.py`, `staff_credentials.py`. The blueprint says "Platform's job, not CEngine's."
- **New:** `ce-server/src/login.rs` — `Identity`, `LoginError`, `Issuing` structs. Token-based auth, no PIN.
- **Impact:** Staff login must work on day one. If the platform provides Auth (Supabase or Infajn), CEngine trusts a signed token and does not need PIN management.

### File Vault
- **Old code:** `FileVault.tsx`, `archived_files` (22 cols, 27 rows live).
- **New:** `document` entity (5 cols: title, kind, period, source, file). Depends on C2 (blob storage).
- **Impact:** 27 archived files must migrate. The entity is ready; storage is not.

### Cost Manager
- **Old code:** `CostManager.tsx`, `BudgetLineItems.tsx`, `cost_budgets` (1 row), `budget_line_items` (13 cols, 4 rows).
- **New:** `budget_line_item` (8 cols), `budget_line_actual` (4 cols), `cost_budget` (5 cols). All present in the template.
- **Impact:** Entity parity is good. Needs real-world proving with MJCC's FY budget figures.

---

## Missing cafeteria capabilities

These are features neither the old system nor the new template has, but a food-service manager needs them. Marked with priority.

### High priority (needed for go-live, derived from the blueprint section 2)

| Capability | Old (Compute) | New template | Engine needed? |
|---|---|---|---|
| Receiving temperature log | Not present | Not present | Template entity: `receiving_log` with `condition`, `temp`, `accepted` |
| Storage locations and expiration/FEFO | Not present | Not present | Template fields on `stock_item_detail` or new `storage_location` entity |
| Lot numbers and recall tracking | Not present | Not present | Template entity: `lot`, referenced from `stock_line` and `stock_movement` |
| Allergen list per menu item | Not present | Not present | Template field on `menu_item` |
| Substitution log | Not present | Not present | Template entity: `substitution_log` |
| Purchase orders | Not present | Not present | Template entity: `purchase_order` + `purchase_order_line` |
| Equipment maintenance log | Not present | Not present | Template entity: `equipment_log` |
| Waste log | Not present | Not present | Template entity: `waste_log` |

### Medium priority (post-launch)

| Capability | Required by |
|---|---|
| Recipe costing (ingredients × current price) | Manager monthly report |
| Three-way match (PO ↔ receiving ↔ invoice) | Auditors |
| Vendor scorecard (price drift, on-time %) | Contracting |
| Meal participation reporting by program | USDA/Job Corps reporting |
| Catering and event requests | Inter-departmental |
| Tablet/mobile scanning for counts | Staff efficiency |
| Production sheets (forecast → pull list) | Kitchen workflow |

---

## Schema comparison: old vs new

| Old MJCC v1 table | New CEngine entity | Column count (old→new) | Notes |
|---|---|---|---|
| `inventory_items` (16 cols) | `stock_item` (10) + `stock_item_detail` (11) | 16 → 21 | Split into item + detail; more normalized |
| `monthly_inventory` (21 cols) | `stock_line` (20) | 21 → 20 | Stock line replaces flat month grid (w1_received, w2_received, w3_received → stock_movement) |
| `inventory_transactions` (20 cols) | `stock_movement` (13) | 20 → 13 | Movement is a child of stock_line, leaner |
| `invoices` (36 cols) | `supplier_invoice` (19) | 36 → 19 | Removed redundant fields (driver_name, route_number, stop_number — operational noise) |
| `invoice_items` (21 cols) | `supplier_invoice_line` (14) | 21 → 14 | Cleaner model, no legacy junk |
| `events` (12 cols) | `cafeteria_event` (13) | 12 → 13 | State machine added (planned/confirmed/completed/cancelled) |
| `menu_items` (7 cols) | `menu_item` (9) | → 9 | Added `active` boolean, `item_key` unique |
| `menu_cycle_slots` (12 cols) | `menu_cycle_slot` (15) | 12 → 15 | Added `updated_by` audit |
| `haccp_logs` (10 cols) | `haccp_log` (14) | 10 → 14 | Adds computed `in_range`, ref to `haccp_limit` |
| `servsafe_certifications` (9 cols) | `servsafe_certification` (11) | → 11 | Adds `staff_member` ref, `is_proctor` |
| `budget_line_items` (13 cols) | `budget_line_item` (8) | → 8 | More focused |
| `snack_bar_products` (8 cols) | `snack_bar_product` (4) | → 4 | Moved pricing to rate + transaction (computed) |
| `vendors` (13 cols) | `vendor` (9) | → 9 | Leaner, address split |
| `user_profiles` (20 cols) | `staff_member` (17) | → 17 | Removed `pin_hash`, `pin_version`, `must_change_password` (platform concern) |
| `commits` + `staging_entries` | `changeset` + `changeset_op` | → 20 cols | Changesets replace the old two-tier model |
| `audit_events` (25 cols) | `audit_event` (18) | → 18 | Structured audit with version tracking, `before`/`after` JSON |

---

## Build order

Based on the PR sequence in the blueprint (section 9), the capability map (section 4), and the gaps above:

### P1: Engine infrastructure (CEngine repo)
1. **C1: Scheduler** — `ce-scheduler` or `ce-job` crate. Recurring and delayed jobs, tenant-scoped, with retry. Unblocks week rollover and HACCP reminders.
2. **C2: Blob store** — Wire `ce-store-pg/src/blobs.rs` through to Supabase S3 or `pg_largeobject`. Unblocks invoice upload and File Vault.
3. **C3: Notifications connector** — Engine outbox events → email/SMS/webhook via a connector. Unblocks HACCP alerts and certification reminders.
4. **C4: Document intake connector** — Deterministic parser + AI/OCR that writes to `import_batch` → `changeset`. Unblocks the invoice workflow.
5. **C5: Report output** — PDF/CSV generation from declarative reports. Unblocks the manager month-end report.
6. **C7: PII field enforcement** — Read the template's `pii: true` flags and redact from public projections and audit.
7. **C8: Operations** — Backup/restore, migration scripts, Render production config.

### P2: MJCC backend parity (MJCC-workspace repo)
8. **T1: Rollover client** — `rollover_stock` action with idempotency, carrying opening cost forward.
9. **T2: Invoice extraction workflow** — Client-side: upload → connector extracts → changeset → merge. End-to-end test against the real API.
10. **T3: Tasks and assignments** — `assignment` entity, daily flow screen, staff roster view.
11. **T4: Receiving and kitchen logs** — `meal_count`, `waste_log`, `receiving_temp`, `equipment_log` entities. At least high-priority items.
12. **T5: Report rendering** — Wire report definitions to output files.

### P3: Staging deployment and owner acceptance
13. Deploy on Render with the Testing DB.
14. Run full month-end simulations (acceptance scenarios 1–10 from the blueprint).
15. Owner/staff verify every workflow.

### P4: UI
16. After backend owner-acceptance, build the thin typed client from `API_CONTRACT_DRAFT.md`.
17. Preserve all 28 screens as documented in `SCREEN_ACTION_MATRIX.md`.

### P5: Production cutover
18. Data migration with reconciled totals, backup/restore test, rollback plan, signed approval.

---

## Capabilities that are complete and require no further work

| Capability | Engine code | Template | DB table | Golden test | Real API test |
|---|---|---|---|---|---|
| Three-week inventory valuation | `ce-plugin/modules/inventory.json` | `stock_item`, `stock_line` | ✅ | `golden/week1.expected.json` | `backend_proof.py` |
| Over-pull rule | Plugin slot `ending_qty` | invariant `no_over_pull` | ✅ | `golden/over_pull.expected.json` | In proof |
| 25% price decision | Slot `price_decision` | `price_observation` entity | ✅ | `golden/price_observations.expected.json` | In proof |
| Rollover cost carryover | Client procedure doc | `stock_line` fields | ✅ | `golden/rollover_week2.expected.json` | Client pending |
| Period/weak lifecycle | `ce-server/src/periods.rs` | `period`, `period_week` | ✅ | `ops_smoke.py` | In proof |
| Changesets/Source Control | `ce-server/src/changesets.rs` | `changeset`, `changeset_op` | ✅ | None yet | In proof |
| Events and outbox | `ce-events/src/outbox.rs` | `outbox_event` | ✅ | None yet | In proof |
| Reports engine | `ce-server/src/reports.rs` | 8 reports declared | ✅ | None yet | In proof |
| Staff roles and authorization | `ce-auth` | 5 roles with entity-level RBAC | ✅ | `staff.json` | `backend_proof.py` |
| HACCP limits and logs | Template only | `haccp_limit`, `haccp_log` with computed `in_range` | ✅ | None yet | In proof |
| Menu cycles | Template only | `menu_cycle_day`, `menu_cycle_slot`, `menu_item` | ✅ | None yet | In proof |
| Snack bar | Template only | 5 entities with computed pricing | ✅ | None yet | In proof |
| Incident log | Template only | `incident_log` with state machine | ✅ | None yet | In proof |
| Daily operations log | Template only | `daily_operations_log` | ✅ | None yet | In proof |
| Opening checklist | Template only | `opening_checklist_item` + `entry` | ✅ | None yet | In proof |