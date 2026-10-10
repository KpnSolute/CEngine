# Template groups and connector types (DRAFT for owner edit)

Status: proposal. Nothing here is decided. CEngine does not build templates. A
template comes from a user's persona, goes into the Templates v1 index, and is
then run by CEngine. These lists say what CEngine must be able to run and
connect to. Edit freely; I will rebuild the capability map from your final lists.

## A. Template groups (15, revised 2026-10-10)

| # | Group | Examples of businesses | What MJCC already covers | Typical extra needs |
| --- | --- | --- | --- | --- |
| 1 | Food service | cafeterias, restaurants, catering, food trucks | inventory, menus, HACCP logs, period close | ordering, table or pickup, supplier orders |
| 2 | Retail and e-commerce | shops, online stores, wholesale | inventory | products and variants, orders, POS, returns, pricing |
| 3 | Hospitality and lodging | hotels, rentals, B&Bs | events, calendar-like | reservations, room or unit availability, housekeeping |
| 4 | Healthcare and wellness | clinics, gyms, therapy, pharmacies | logs, compliance | appointments, patient or client records, consent, private data rules |
| 5 | Education and training | schools, tutoring, job training | users, forms | enrolment, classes and schedules, attendance, grading, certificates |
| 6 | Construction and trades | contractors, electricians, builders | forms, staging | projects and jobs, quotes, site logs, safety checks, subcontractors |
| 7 | Professional services | legal, accounting, consulting, agencies | approvals | clients, time tracking, billing, document workflow, matters or cases |
| 8 | Real estate and property | agents, landlords, property managers | records, logs | listings, leases, maintenance requests, rent collection |
| 9 | Manufacturing and production | workshops, packaging, fabrication | inventory, logs | bill of materials, work orders, quality checks, batches |
| 10 | Logistics and transportation | delivery, fleets, warehousing | inventory | shipments, routes, drivers, tracking, proof of delivery |
| 11 | Public sector and nonprofit | agencies, charities, clubs, faith groups | events, forms, compliance, audit | members, donations, volunteers, grants, cases, permits, public records |
| 12 | Events and entertainment | venues, promoters, studios | events, public pages | ticketing, scheduling, vendors, capacity |
| 13 | Field and home services | cleaning, repair, salons, landscaping | forms, logs | dispatch, appointments, quotes, invoices, mobile use |
| 14 | Finance, insurance and payroll | brokers, lenders, bookkeeping, payroll providers | audit, approvals | accounts, policies or loans, KYC, pay runs, deductions, tax filings, strict rules |
| 15 | Human resources | HR teams, staffing agencies, recruiters | users, roles, forms | hiring and applicants, onboarding, time off, performance, benefits, employee records |

Changes from the first draft: Human Resources added as its own group; payroll placed under Finance (as the owner asked). To stay at 15, I merged Government and Nonprofit into one group; split them back if you want both and drop another. Not on the list (swap in if you prefer): agriculture, media, sports, telecom, energy.

Note: HR and payroll are also built-in tools for every group (staff exist everywhere). The HR and Finance groups are for businesses whose main work is HR or payroll.

## B. Built-in tools every group gets (what CEngine should already have)

Present in MJCC today: users and roles, records with states, forms, daily and
compliance logs, audit trail, period close and locking, review-and-merge,
reports and dashboards, public pages, AI data entry, files.

Missing and needed across groups: calendar and booking, tasks and reminders,
notifications (email, SMS, push), customers and contacts, **HR and people**
(employees, roles, time off, timesheets, onboarding), **payroll** (pay periods,
hours to pay, deductions, pay runs, records), invoicing and
payments, documents and e-signature, search, import and export, comments and
attachments, multi-location, scheduling and recurring jobs, webhooks out,
usage metering. (My guess; not yet checked against the MJCC code.) HR and payroll are built-in tools
for every group, because every business has staff. Payroll providers are
connectors (10b). Payroll moves money, so it follows the read-only-first rule in D.

## C. Connector types

A connector is how CEngine pulls data from, or pushes data to, a service outside
the template. CEngine defines the connector contract and the data shapes.
Infajn is the service that talks to the real provider. Templates declare which
connector types they need, never provider names or credentials.

| # | Type | Direction | Examples of what it gives a template |
| --- | --- | --- | --- |
| 1 | Auth and identity | pull | single sign-on, user directory, MFA |
| 2 | Storage | both | files and images, backups, document stores |
| 3 | Banking | pull (read first) | account balances, bank transactions, reconciliation |
| 4 | Payments and transactions | both | card or wallet charges, payouts, refunds, subscriptions |
| 5 | Accounting | both | invoices, ledgers, tax records |
| 6 | Email and messaging | push | email, SMS, chat notifications |
| 7 | Calendar and scheduling | both | calendars, availability, bookings |
| 8 | CRM and customer data | both | contacts, deals, support tickets |
| 9 | Commerce and POS | both | orders, products, point-of-sale sales |
| 10 | ERP and purchasing | both | purchase orders, suppliers, stock sync |
| 10b | HR and payroll providers | both | staff records, hours, pay run submission, pay stubs |
| 11 | Logistics and shipping | both | carriers, labels, tracking |
| 12 | Maps and location | pull | addresses, geocoding, routes |
| 13 | Documents and e-signature | both | contracts, signatures, templates |
| 14 | Analytics and BI | push | metrics export, dashboards |
| 15 | AI and language models | both | extraction, summaries, data entry |
| 16 | Identity verification (KYC) | pull | ID checks, compliance screening |
| 17 | Devices and IoT | pull | sensors, scanners, printers, temperature probes |
| 18 | Generic: webhooks, API, file and database import | both | anything with no dedicated type |

## D. Rules every connector must follow (proposed)
0. **Infajn provisions, CEngine controls.** All resources, including AI models, are provisioned by Infajn. CEngine is a controller: it decides whether a template may use a resource, applies limits and approvals, meters and audits, and uses the result. (Owner, 2026-10-10.)
1. Credentials live in a vault; templates and manifests only hold a reference.
2. Pulled data is normalised into the template's declared entities, with source, time and an idempotency key on every record.
3. Reads are the default. Anything that moves money or changes outside data is an explicit action with approval and an audit entry.
4. Sync is incremental and resumable; one failed provider never blocks the template.
5. Banking and payments start read-only and need a separate approval before any write.
6. Every connector call is metered and logged per tenant.

## Open questions
1. Final group list: OK with the merge of Government and Nonprofit to make room for HR?
2. Are 18 connector types too many to start? Suggested first wave: auth, storage, banking (read), payments, email and messaging, generic webhook/import.
3. ~~Is Infajn the only place provider calls happen?~~ **Answered by owner 2026-10-10: yes.** Infajn makes all provider calls and CEngine uses the results. CEngine never calls a provider directly.
