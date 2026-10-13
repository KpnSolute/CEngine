# CEngine staging on Render (preparation only)

MJCC staging: the Blueprint now runs the MJCC template. Upload `template/mjcc.template.json` from MJCC-workspace as the Render secret file `mjcc.template.json`, and `staff.json` (first admin) as `staff.json`; `CE_TEMPLATE` and `CE_MODULE` replace `CE_MANIFEST`. The same `CE_TEMPLATE`, `CE_MODULE`, `CE_INSTALLATION` and `CE_SCHEMA` must be used for `ce-serve install`. The step-by-step guide for the owner is `mjcc-cengine/go-live-staging-runbook.md` in the project files.

Nothing here has been deployed. This prepares the files; the owner applies them. Staging only: no production database, no production credentials, no domain or DNS work.

## Service

| Item | Value |
| --- | --- |
| Blueprint | `render.yaml` at the repository root |
| Service name | `cengine-staging` (web service) |
| Runtime | `docker` |
| Dockerfile | `./Dockerfile` (the repo's existing image), context `.` |
| Plugin-phase Dockerfile | `./deploy/render/Dockerfile` (see "Plugin bytes and the approval record") |
| Plan | `free` (see "Free-plan limits") |
| Region | OWNER DECISION (commented out in the Blueprint; Render's default is applied if left unset, and it cannot be changed later) |
| Health check path | `/_meta/health/ready` (liveness also exists: `/_meta/health/live`) |
| Auto deploy | off (`autoDeployTrigger: "off"`); deploys are manual |
| Bind | `0.0.0.0:8080` via `CE_BIND`, with `PORT=8080` set to match |

PORT: `ce-serve` does not read `PORT`. It binds `CE_BIND` (default `127.0.0.1:8080`; the image sets `0.0.0.0:8080`). Render routes to the `PORT` value, so the Blueprint sets both to 8080. If you change one, change the other. A loopback bind would fail the health check.

## Environment variable names

Values are never in the repo. Secrets are `sync: false`: Render asks for them at Blueprint creation.

| Name | Purpose | Where the value comes from |
| --- | --- | --- |
| `PORT` | Port Render routes to | Blueprint literal `8080` |
| `CE_BIND` | Listen address | Blueprint literal `0.0.0.0:8080` |
| `CE_INSTALLATION` | Installation id used in URLs `/v1/{id}/...` | Blueprint literal `mjcc-staging` (owner may rename) |
| `CE_MANIFEST` | A finished manifest JSON, for a generic (non-MJCC) staging run. The MJCC Blueprint does not set it; use `CE_TEMPLATE` and `CE_MODULE` instead (never both) | A Render secret file, if used |
| `CE_POOL_SIZE` | Max DB connections | Blueprint literal `5`; keep below the database's connection limit |
| `CE_DATABASE_URL` | Postgres connection string | Owner: the external non-production database (sync: false) |
| `CE_JWT_KEY` | HS256 signing key, at least 32 bytes | Owner generates a staging-only key, e.g. `openssl rand -hex 32` (sync: false) |
| `CE_DB_TLS` | `off`, `webpki` or `ca` | Owner, depending on the database (sync: false). Never `off` over a public network |
| `CE_DB_CA` | Path to CA PEM, only with `CE_DB_TLS=ca` | Owner uploads the CA as a Render secret file, e.g. `/etc/secrets/db-ca.pem` (sync: false) |
| `CE_SCHEMA` | Postgres schema name (default `ce_` + installation id) | Optional on first boot; MUST be a new name for the plugin phase, and the same on the install command and the service |
| `CE_STAFF_FILE`, `CE_TENANT` | Turn on staff PIN sign-in at `POST /v1/{id}/_auth/token`. `CE_STAFF_FILE` is a staff list the owner keeps as a Render secret file, e.g. `/etc/secrets/staff.json`: `{"users":[{"username":"chef","pin_hash":"kpn-scrypt$1$...","roles":["manager"]}]}`. `CE_TENANT` is the one lowercase uuid every token is for. Make a hash with `ce-serve staff hash` (reads the PIN on stdin); hashes from the legacy backend work unchanged. | Optional. Unset, sign-in answers 503. The server refuses to start on a bad list |
| `CE_STAFF_SOURCE`, `CE_STAFF_ADMIN_ROLE`, `CE_MIN_PIN_LEN` | `CE_STAFF_SOURCE=postgres` keeps staff accounts in the installation's schema (`_ce_staff`, with a change trail in `_ce_staff_event`). `CE_STAFF_FILE` then only seeds an empty table once (the first admin must come from it; the server refuses to start with no accounts). After that people are created, switched off and re-PINned over the API: `GET/POST /_staff`, `PATCH /_staff/{user}`, `POST /_staff/{user}/reset_pin`, and each person changes their own PIN at `POST /_auth/pin`. A new or reset account must pick its own PIN at first sign-in (the token it gets opens only that route). `/_staff` needs the `CE_STAFF_ADMIN_ROLE` role (default `admin`); nobody can grant or touch a role above their own or change their own roles. `CE_MIN_PIN_LEN` (default 6) is the shortest PIN. | Optional. Unset, the read-only file is used and the admin routes answer 409 |
| `CE_PUBLIC_WORKSPACE` | The workspace name in public projection URLs (`/public/{installation}/{workspace}/{projection}`, no sign-in), served for `CE_TENANT`. MJCC's public menu and meal times use it. Lowercase letters, digits, hyphens. | Optional. Unset, every public URL answers 404 |
| `CE_FILES` | `postgres` stores uploaded files (invoice scans, documents) in the installation's schema, table `_ce_blobs`, so they survive Render's free-plan restarts. Created on start if missing. | Optional. Unset, `PUT/GET /_files/...` answer 503. Uploads whose record write failed, and replaced files, leave bytes behind; `ce-serve files sweep <hours>` deletes bytes no record uses that are older than that (it also drops the old bytes of replaced files from audit history, so run it by hand, not on a timer) |
| `CE_PROVIDER_URL`, `CE_PROVIDER_TOKEN`, `CE_PROVIDER_TIMEOUT_SECS` | The host gateway that makes AI calls for invoice import (`POST /_ai/entry`). The URL must be https; the token is the gateway's bearer token (set it as a secret, never in the blueprint). CEngine itself holds no AI key. The wire format is in `crates/ce-provider-http`. | Optional. Unset, AI import answers 503 |
| `CE_TOKEN_TTL_SECS`, `CE_LOGIN_MAX_FAILED`, `CE_LOGIN_LOCKOUT_SECS`, `CE_ROLE_LADDER` | Token life (default 43200), wrong PINs before a lock (5), lock length (900), role order lowest first (`staff,assistant,manager,admin,sudo`; a role also grants those below) | Optional. The lock count lives in the process, so run one instance |
| `CE_TEMPLATE`, `CE_MODULE` | The MJCC template (a Render secret file, `/etc/secrets/mjcc.template.json`) and the module `ce-serve` adds to it (`business.inventory`). Never together with `CE_MANIFEST` | Set in the Blueprint |
| `CE_PLUGIN`, `CE_PLUGIN_ROOT`, `CE_INSTALLS` | Plugin phase only | See below |

## Database requirements

- A separate, non-production Postgres. Never the MJCCv1 production project and never its service-role key.
- Render's free Postgres is not suitable: it is limited and expires. Use an external non-production Postgres (for example a dedicated staging Supabase project or any managed Postgres) that is reachable from Render's outbound addresses.
- TLS: use `webpki` if the server certificate chains to a public root, `ca` with `CE_DB_CA` if it uses a private CA (Supabase uses its own CA; verify before choosing). `off` is acceptable only on a private network. Which mode a given provider needs was not verified here.
- The database user needs rights to create a schema (`ce-serve install` creates `ce_<installation>`) and to read and write in it.

## Free-plan limits

- The service sleeps when idle; the first request after sleep waits for a cold start, so run the readiness check first and allow time. A sleeping service looks like a failed smoke test.
- The filesystem is ephemeral and there is no persistent disk (disks need a paid plan) and no Render Shell on free. Anything written at runtime is lost on every restart or deploy.
- Secret files are available and read-only at `/etc/secrets/`.
- No pre-deploy command is available, so the schema install is never part of the deploy (see below).
- Free instance hours and build minutes are limited; builds of this Rust image are long.
- Free is for staging only. Do not treat its availability as meaningful.

## Plugin bytes and the approval record

`ce-serve` verifies the plugin on every start against the digest in the install record (`CE_INSTALLS`), and refuses to start if either is missing or changed. Both therefore must exist at start, on a filesystem that is ephemeral here.

Chosen option: bake the pinned plugin bytes and the approval record into the image. Why:
- A free service has no persistent disk, and a disk would also force a single instance with no zero-downtime deploys.
- An image is immutable, so what runs is exactly what was approved and reviewed in git; a restart cannot lose or silently change it.
- The approval is a recorded owner act (`ce-serve plugin approve <slug> <digest>`) made locally, with the output committed. Rollback is a redeploy of an earlier commit.

Alternative, only on a paid plan: a persistent disk mounted at e.g. `/data`, with `CE_PLUGIN_ROOT` and `CE_INSTALLS` under it. Constraints: one instance only, no zero-downtime deploys, the mount is root-owned while the image runs as uid 10001 (it would need a permissions fix), and plugin bytes would have to be placed by hand through the shell. Not recommended for staging.

Procedure for the plugin phase (after the first boot below works):
1. Put the plugin folder at `deploy/render/plugins/.cengine/plugins/<id>/` (the plugin lives in a separate private repo; copy the reviewed bytes at the exact pinned version).
2. Locally: `CE_PLUGIN_ROOT=deploy/render/plugins CE_INSTALLS=deploy/render/install-record/installs.json ce-serve plugin digest <id>`, review the digest, then `... plugin approve <id> <digest>`. Commit the plugin folder and `installs.json` to a staging branch.
3. Change the Blueprint service to `dockerfilePath: ./deploy/render/Dockerfile` and set `CE_PLUGIN`, `CE_PLUGIN_ROOT=/opt/ce/plugins`, `CE_INSTALLS=/opt/ce/install-record/installs.json`. The template JSON must also be in the image; the real MJCC template is not in this repo (see questions). The plugin-phase Dockerfile bakes the repo's `spec/examples/inventory.json` as `/opt/ce/manifest.json` only.
4. Run `ce-serve install` (it composes from the same approved bytes) with a NEW `CE_SCHEMA` (for example `ce_mjcc_staging_plugin`), because the first-boot schema already exists and install is not idempotent. Set the same `CE_SCHEMA` on the Render service, then deploy.

The private plugin repo is not readable from the build, so no credential is placed in the build. If the plugin must not be committed to this repo, the owner needs a different mechanism (a decision for the owner).

## First boot (no plugin) and install/migrate order

`ce-serve install` is not idempotent: a second run on an existing schema fails (`install failed`), and `ce-serve` itself refuses to start if the schema does not exist. There is no migration command in `ce-serve` in v0.2.1 beyond `install`; schema changes need a new installation (new `CE_SCHEMA`) or a reviewed migration, which is outside this PR.

Order:
1. Create the staging database; collect the connection URL, TLS mode and CA.
2. Run the install once, from the owner's machine or CI, against the staging database. With the image: `docker run --rm -e CE_DATABASE_URL=... -e CE_DB_TLS=... -e CE_JWT_KEY=... -e CE_INSTALLATION=mjcc-staging -e CE_SCHEMA=ce_mjcc_staging -e CE_TEMPLATE=/mjcc.template.json -e CE_MODULE=business.inventory -v ./mjcc.template.json:/mjcc.template.json:ro cengine install` (the template, module, installation and schema must be exactly what the service uses; installing from a different manifest creates the wrong tables) (with `CE_DB_TLS=ca` also add `-v ./db-ca.pem:/db-ca.pem:ro -e CE_DB_CA=/db-ca.pem`; without `CE_DB_CA` the install exits with `CE_DB_CA is required`). It prints `installed schema ce_mjcc_staging` (or the name derived from the installation id).
3. In the Render Dashboard create the Blueprint, enter the `sync: false` values, and upload the secret files `mjcc.template.json` (the same bytes used in step 2) and `staff.json` (the first admin), and set `CE_SCHEMA` and `CE_TENANT`.
4. Deploy manually, wait for the health check, run the smoke tests.

Installing from Render itself is not possible on the free plan (no shell, no pre-deploy command).

## Smoke tests

Replace `$HOST` with the service's `onrender.com` host and `$INST` with `CE_INSTALLATION`. These routes exist in `crates/ce-server/src/lib.rs`.

```sh
curl -fsS https://$HOST/_meta/health/live        # {"status":"ok"}
curl -fsS https://$HOST/_meta/health/ready       # {"status":"ok"}
curl -s -o /dev/null -w '%{http_code}\n' https://$HOST/v1/$INST/item          # 401 without a token
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://$HOST/v1/$INST/_auth/token   # 503 without CE_STAFF_FILE, a 4xx with it (no credentials sent)
```

Authenticated checks need a token. With `CE_STAFF_FILE` set, `POST /v1/$INST/_auth/token` with `{"username":"...","pin":"..."}` returns one; without it the route returns 503 and a staging token must be signed by the owner with `CE_JWT_KEY` (HS256; claims `sub`, `tenant` = a lowercase uuid, `roles` containing a role from the manifest, `exp`). Mint it locally and never paste the key anywhere else:

```sh
TOKEN=$(CE_JWT_KEY=... python3 - <<'PY'
import hmac,hashlib,base64,json,time,os
b=lambda x:base64.urlsafe_b64encode(x).rstrip(b'=')
h=b(json.dumps({"alg":"HS256","typ":"JWT"}).encode())
p=b(json.dumps({"sub":"smoke","tenant":"11111111-1111-4111-8111-111111111111","roles":["manager"],"exp":int(time.time())+600}).encode())
print((h+b'.'+p+b'.'+b(hmac.new(os.environ["CE_JWT_KEY"].encode(),h+b'.'+p,hashlib.sha256).digest())).decode())
PY
)
curl -fsS -H "Authorization: Bearer $TOKEN" https://$HOST/v1/$INST/item
curl -fsS -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' -H 'Idempotency-Key: smoke-1' \
  -d '{"name":"smoke item","unit_cost":"1.5000"}' https://$HOST/v1/$INST/item        # 201
curl -fsS -H "Authorization: Bearer $TOKEN" https://$HOST/_meta/manifest
```

The `item` bodies above match the generic `spec/examples/inventory.json` (role `manager`); for the MJCC template use the entities in its README or `tests/ops_smoke.py` in MJCC-workspace instead. Run on a locally built binary against a throwaway local Postgres on 2026-10-12: install succeeded, the second install failed, live and ready returned 200, an unauthenticated item request returned 401, `_auth/token` returned 503, a minted token listed items (200, empty) and created one (201). This has not been run against Render.

## Known behaviours to be aware of

- `/_meta/health/ready` always answers `{"status":"ok"}`; it does not check the database. A broken database shows up as failing authenticated routes, not as a failed health check. Always include an authenticated request in the smoke test.
- `ce-serve` shuts down gracefully only on SIGINT (Ctrl-C), not SIGTERM, which is what Render sends. As PID 1 in a container it may ignore SIGTERM and be killed after Render's grace period; in-flight requests can be cut off on deploy. Not verified inside a container (no Docker daemon was available) and not changed in this PR.

## Rollback

1. Deploy a previous known-good commit: Dashboard, Events, pick the earlier deploy, Rollback (or deploy the earlier commit manually). Auto deploy is off, so nothing changes on its own.
2. Env var mistakes: fix in the Dashboard and redeploy.
3. Plugin: `ce-serve plugin rollback <id>` locally on the committed record needs the older version's bytes in the plugin folder; commit both and redeploy. `disable <id>` stops serving it (the service then refuses to start with that plugin set, so unset `CE_PLUGIN` or revert the commit).
4. Database: the staging schema can be dropped and reinstalled (`drop schema ... cascade`, then `ce-serve install`) because it is throwaway data. Only do this on the staging database.
5. Suspend the Render service to stop it entirely.

## Questions only the owner can answer

1. Region (must be set before creation; keep it near the database).
2. Which external staging Postgres, and its TLS mode and CA (Supabase staging project? other?).
3. The installation id and schema name for staging (`mjcc-staging` assumed).
4. The real MJCC template JSON and module for the plugin phase; it is not in this repo. Until then staging serves the generic inventory example.
5. How the private MJCC plugin bytes reach this repo (copied and committed to a staging branch here, or another mechanism), and who runs `plugin approve`.
6. Who mints staging tokens and which roles/tenant uuid to use, since `ce-serve` has no login adapter.
7. Whether to accept the free plan's sleep and cold starts for acceptance runs, or move to a paid plan (disk, always on).
8. Branch to deploy (Blueprint says `main`) and whether a staging branch is preferred.
9. Whether SIGTERM handling should be added to `ce-serve` (small Rust change, separate PR).

## Secret files and the service user

Render mounts secret files readable by group 1000. Both images add the non-root `ce` user to a group with gid 1000 so the template, staff and `CE_DB_CA` secret files can be read. Not verified on Render (no Docker daemon here).
