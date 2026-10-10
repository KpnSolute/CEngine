# CEngine capability map (DRAFT, from the MJCC code audit)

Status: draft for owner review. Evidence is from the KpnSolute/Compute source
read 2026-10-10: 25 route files, about 196 handlers, 55 backend migration files,
about 110 tables, and the React components list. "Evidence" names tables or
routes. Where I could not confirm something it is marked **unverified**.

Reading guide:
- **Built-in** = every template on CEngine should get it without writing it.
- **Template** = business-specific; stays in a template (manifest data), not the engine.
- **Missing** = MJCC does not have it (or I found no evidence); CEngine needs it for the other 14 groups.

## 1. What MJCC already has, sorted

| Area | Evidence in MJCC | Verdict for CEngine |
| --- | --- | --- |
| Users, roles, permissions | `user_profiles`, `role_permissions`, `permission_scopes`, `routes/users.py`; roles staff, assistant, manager, admin | **Built-in** (roles as manifest data) |
| Staff PIN and admin sign-in, sessions, login throttle | `staff_sessions`, `staff_login_throttle`, `routes/auth.py` | Platform's job, not CEngine's (CEngine trusts a signed token). Keep as an adapter during migration |
| Tenants, memberships, sites | `tenants`, `tenant_memberships`, `tenant_tree_nodes`, `workspace_sites` | Platform and Infajn own it; CEngine only enforces the tenant scope |
| Records with states and approvals | `staging_area`, `pending_changes`, `month_status`, `week_status` | **Built-in** (state model, transitions, locks) |
| Review, merge, revert of changes | `commits`, `commit_changes`, `pull_requests`, `routes/sourcectrl.py` (about 2,350 lines for inventory only) | **Built-in** as generic changesets |
| Period close and freeze | `month_periods`, `monthly_snapshots`, immutability guard | **Built-in** (freeze windows, snapshots) |
| Audit trail | `audit_log`, `audit_events`, `inventory_audit_log`, `credential_access_audit` | **Built-in** (automatic, every write) |
| API call logs and error logs | `api_logs`, `error_logs`, `routes/api_logs.py` | **Built-in** (metering and diagnostics) |
| Files and documents | `uploads`, `documents`, `archived_files`, `file_archive.py`, `archive_storage.py` | **Built-in** (tenant-prefixed storage, hashes) |
| Notifications and email | `notifications.py`, `email_log`, `email_templates` | **Built-in** engine with a connector for delivery (via Infajn) |
| Events out (webhooks, CloudEvents) | `kpnsolute_events.py`, ledger | **Built-in** outbox |
| Dashboards, reports, print and CSV | `Reports.tsx`, `Dashboard`, `routes/data.py` | **Built-in** (declared reports and exports) |
| Public pages with limited fields | `public_menu.py`, `menu_feedback_summary` | **Built-in** (public projections) |
| AI-assisted data entry | `ai/` (engine, parser, invoice_parser, mapper, diff), `routes/data_entry.py` | **Built-in** as an AI-entry service through a connector (AI models) |
| AI agent chat | `routes/agent.py`, `agent_threads`, `agent_conversations`, `agent_usage` | **Built-in** or a separate service (open question) |
| Budgets and cost tracking | `cost_budgets`, `budget_line_items`, `routes/cost.py` | **Built-in** (budgets vs actuals) |
| Price and SKU review queues | `price_review_queue`, `sku_review_queue` | Generic "review queue" **built-in**; the rules are template |
| Vendors and suppliers | `vendors`, `suppliers` | **Built-in** (contacts and parties) |
| Inventory | `inventory_items`, `inventory_transactions`, `weekly_counts`, `reorder_alerts`, barcodes | Common to many groups; **built-in as a stock module** with food rules in the template |
| Barcodes and QR | `barcodes`, `item_barcodes`, `qr_codes` | **Built-in** |
| Provisioning of workspaces | `provisioning_jobs`, `provisioning_steps`, `workspace_projects`, `project_blueprint_versions` | Infajn and Compute (persona), not CEngine |
| Calendar of events | `events`, `EventsCalendar.tsx` | Basic only; see Missing |
| Daily checklists and logs | `daily_operations_logs`, `opening_checklist_items`, `incident_logs` | **Built-in** as generic logs and checklists |
| Temperature and HACCP logs, ServSafe certs | `haccp_logs`, `servsafe_certifications` | **Template** (food service) |
| Menus and cycles, meal periods | `menu_*` tables, `CycleMenu.tsx` | **Template** (food service) |
| Snack bar sales and rates | `snack_bar_*` tables | **Template** (food service); POS part is Missing as built-in |
| Invoices (as incoming) | `invoices`, `invoice_items` | Incoming only; see Missing for outgoing invoicing |

## 2. What every group needs that MJCC does not have (**Missing**)

Confirmed absent from the table list and routes unless marked unverified.

1. Calendar and booking: appointments, availability, recurring events (MJCC has only a simple event list).
2. Customers and contacts as first-class records (people outside the staff list).
3. Outgoing invoicing, quotes and payments.
4. HR and people: employees, time off, timesheets, onboarding. (No HR tables found.)
5. Payroll: pay periods, pay runs, deductions.
6. Tasks, assignments and reminders (only `flow_assignments` exists, unclear scope; **unverified**).
7. Documents and e-signature.
8. Messaging between users and with customers, comments on records.
9. Search across all records.
10. Import and export with mapping (only inventory invoice import found).
11. Multi-location and multi-currency handling.
12. Connectors: no general connector layer. MJCC has `github_sync` and AI provider keys only.
13. Scheduling of recurring jobs run by the engine (**unverified** whether MJCC has a scheduler).
14. Per-field privacy for personal and health data.
15. Dispatch and mobile field work (routes, GPS).
16. Reservations and capacity (rooms, tables, seats).

## 3. Per-group extra needs (beyond the built-in set)

These are what the 15 groups add. They are template content on top of the built-ins, except where marked.

| Group | Needs beyond built-ins |
| --- | --- |
| Food service | HACCP logs, menus, recipes, supplier ordering (MJCC covers most) |
| Retail and e-commerce | products and variants, POS, returns, pricing, channel sync |
| Hospitality and lodging | reservations and unit availability, housekeeping |
| Healthcare and wellness | appointments, client records, consent, strict privacy |
| Education and training | enrolment, classes, attendance, grades, certificates |
| Construction and trades | jobs, quotes, site logs, subcontractors, safety checks |
| Professional services | matters or cases, time tracking, billing |
| Real estate and property | listings, leases, maintenance requests, rent |
| Manufacturing | bill of materials, work orders, batches, quality |
| Logistics and transportation | shipments, routes, drivers, tracking |
| Public sector and nonprofit | members, donations, volunteers, grants, permits, public records |
| Events and entertainment | ticketing, capacity, vendors |
| Field and home services | dispatch, quotes, invoices, mobile |
| Finance, insurance and payroll | accounts, policies or loans, KYC, pay runs, tax filings |
| Human resources | applicants, onboarding, performance, benefits |

## 4. Proposed build order for CEngine built-ins

Based on how many groups need each item and how much MJCC already proves.

1. **Core already specified:** records, states, roles, audit, changesets, period close, events, files, public projections.
2. **Next:** customers and contacts, calendar and booking, tasks and reminders, notifications, search, import and export.
3. **Then:** invoicing and payments, HR and payroll tools, documents and e-signature, messaging and comments.
4. **With connectors (Infajn):** the six first-wave connector types in `groups-and-connectors.md`.

## Controller rule (owner, 2026-10-10)
CEngine is a **controller**. Infajn provisions the resources (including AI/model resources, connectors, storage, compute); CEngine uses and directs them: which resource a template may use, limits, metering, approvals, audit. CEngine does not host models or run provider infrastructure. So AI data entry and agent chat are CEngine-controlled uses of Infajn-provisioned AI resources, not AI services CEngine runs itself.

## Open questions
1. ~~Is the AI agent chat part of CEngine or a separate service?~~ Settled by the controller rule: CEngine controls the use; Infajn provisions the AI resource. Still open: where the chat interface itself lives (Platform or a template).
2. Are customers, calendar and invoicing separate built-in modules, or one "business records" module?
3. Do you agree with the build order in section 4?
