//! `ce-serve`: runs one CEngine installation.
//!
//! ```text
//! ce-serve install   create the installation's Postgres schema (fails if it exists)
//! ce-serve [serve]   open the schema and serve the HTTP API (default)
//! ce-serve files sweep <hours>  delete stored file bytes no record uses and older than that many hours
//! ce-serve staff hash  read a PIN on stdin, print the hash to put in the staff list
//! ce-serve plugin status | digest <slug> | approve <slug> <digest>
//!                    | rollback <slug> | disable <slug> | enable <slug>
//! ```
//!
//! Configuration is by environment variable; secrets are never printed.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `CE_DATABASE_URL` | Postgres connection string (required) |
//! | `CE_MANIFEST` | path to the template manifest JSON (this or `CE_TEMPLATE`) |
//! | `CE_TEMPLATE` | path to a template JSON to compose with a module (instead of `CE_MANIFEST`) |
//! | `CE_MODULE` | module to compose into the template, e.g. `business.inventory` (with `CE_TEMPLATE`) |
//! | `CE_PLUGIN` | id of the customer plugin to run on the module; it must be approved, else the server refuses to start |
//! | `CE_PLUGIN_ROOT` | folder holding `.cengine/plugins/<id>/plugin.toml` (with `CE_PLUGIN` and the `plugin` commands) |
//! | `CE_INSTALLS` | path to the install record, the approvals the `plugin` commands write (with `CE_PLUGIN`) |
//! | `CE_JWT_KEY` | HS256 signing key, at least 32 bytes (required) |
//! | `CE_INSTALLATION` | installation id used in URLs, `/v1/{id}/...` (required) |
//! | `CE_SCHEMA` | Postgres schema name (default `ce_` + the installation id) |
//! | `CE_POOL_SIZE` | most database connections open at once (default 8) |
//! | `CE_DB_TLS` | `off` (default), `webpki` (public roots) or `ca` (needs `CE_DB_CA`, a PEM file) |
//! | `CE_DB_CA` | path to the CA certificate(s) the database's certificate must chain to |
//! | `CE_STAFF_FILE` | staff list JSON (`{"users":[{"username","pin_hash","roles"}]}`); turns on `POST /v1/{id}/_auth/token`, else it answers 503 |
//! | `CE_TENANT` | the one tenant every issued token is for, a lowercase uuid (required with `CE_STAFF_FILE`) |
//! | `CE_TOKEN_TTL_SECS` | token lifetime, 1 to 86400 (default 43200, 12 hours) |
//! | `CE_LOGIN_MAX_FAILED` | wrong PINs before a username locks (default 5) |
//! | `CE_LOGIN_LOCKOUT_SECS` | how long it stays locked (default 900) |
//! | `CE_ROLE_LADDER` | comma-separated roles, lowest first (default `staff,assistant,manager,admin,sudo`); a role also grants those below it |
//! | `CE_FILES` | `postgres` keeps uploaded files in the installation's schema (`_ce_blobs`); unset leaves file routes answering 503 |
//! | `CE_PROVIDER_URL` | the host gateway's https endpoint for provider calls (AI invoice import); unset leaves them answering 503 |
//! | `CE_PROVIDER_TOKEN` | bearer token the gateway expects (a secret; required with `CE_PROVIDER_URL`) |
//! | `CE_PROVIDER_TIMEOUT_SECS` | longest one provider call may take, 1 to 300 (default 30) |
//! | `CE_STAFF_SOURCE` | `postgres` keeps staff accounts in the installation's schema (`_ce_staff`) so they can be created, switched off and re-PINned over the API; `CE_STAFF_FILE` then only seeds an empty table. Unset keeps the read-only file |
//! | `CE_PUBLIC_WORKSPACE` | the workspace name public projection URLs answer for (`/public/{installation}/{workspace}/{projection}`), served for `CE_TENANT`; unset keeps every public URL at 404 |
//! | `CE_STAFF_ADMIN_ROLE` | the role that may use `/_staff` (default `admin`) |
//! | `CE_MIN_PIN_LEN` | shortest PIN anyone may set, 4 to 64 (default 6) |
//! | `CE_BIND` | listen address (default `127.0.0.1:8080`) |

use ce_auth::Verifier;
use ce_server::{router, AppState};
use ce_store_pg::TypedPgStore;
use std::process::ExitCode;

fn var(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("{name} is required")),
    }
}

/// A Postgres schema name made from the installation id: lower-case letters, digits, `_`.
fn default_schema(installation: &str) -> String {
    let body: String = installation
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("ce_{body}")
}

fn db_tls() -> Result<ce_store_pg::Tls, String> {
    let mode = std::env::var("CE_DB_TLS").unwrap_or_default();
    match mode.as_str() {
        "" | "off" => Ok(ce_store_pg::Tls::Off),
        "webpki" => Ok(ce_store_pg::Tls::WebPki),
        "ca" => {
            let path = var("CE_DB_CA")?;
            std::fs::read(&path)
                .map(ce_store_pg::Tls::Ca)
                .map_err(|e| format!("cannot read {path}: {e}"))
        }
        _ => Err("CE_DB_TLS must be off, webpki or ca".to_string()),
    }
}

fn schema_exists(url: &str, tls: &ce_store_pg::Tls, schema: &str) -> Result<bool, String> {
    let mut client = ce_store_pg::connect(url, tls)
        .map_err(|e| format!("cannot connect to the database: {e:?}"))?;
    let rows = client
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .map_err(|e| format!("cannot check schema {schema}: {e}"))?;
    Ok(!rows.is_empty())
}

const ENGINE: &str = env!("CARGO_PKG_VERSION");

fn engine() -> semver::Version {
    semver::Version::parse(ENGINE).expect("crate version is semver")
}

fn read_installs(path: &str) -> Result<ce_plugin::Installed, String> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ce_plugin::Installed::new())
        }
        Err(e) => return Err(format!("cannot read {path}: {e}")),
    };
    // Bounded read: a huge or corrupt file is refused without being loaded whole.
    let max = ce_plugin::MAX_RECORD_BYTES as u64;
    let mut text = String::new();
    file.take(max + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    if text.len() as u64 > max {
        return Err(format!("{path} is not a valid install record: too large"));
    }
    ce_plugin::Installed::from_json(&text)
        .map_err(|e| format!("{path} is not a valid install record: {e}"))
}

/// Write the record through a temporary file in the same folder, so a crash never leaves a
/// half-written approval list.
fn write_installs(path: &str, installed: &ce_plugin::Installed) -> Result<(), String> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, installed.to_json()).map_err(|e| format!("cannot write {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {path}: {e}"))
}

/// `ce-serve plugin ...`: the owner's approval commands. Approving needs the digest that was
/// reviewed, typed or pasted, and refuses when the file on disk no longer has it.
fn plugin_command(args: &[String]) -> Result<(), String> {
    let root = std::path::PathBuf::from(var("CE_PLUGIN_ROOT")?);
    let installs = var("CE_INSTALLS")?;
    let sub = args.first().map(String::as_str).unwrap_or("");
    let slug = args.get(1).map(String::as_str);
    fn need<'a>(s: Option<&'a str>, sub: &str) -> Result<&'a str, String> {
        s.ok_or_else(|| format!("plugin {sub} needs a plugin id"))
    }
    let mut inst = read_installs(&installs)?;
    match sub {
        "status" => {
            for id in inst.plugin_ids() {
                match inst.active(id) {
                    Some(p) => println!("{id} {} {} active", p.version, p.digest),
                    None => println!("{id} disabled ({} approvals kept)", inst.history(id).len()),
                }
            }
            Ok(())
        }
        "digest" => {
            let d = ce_plugin::digest_of(&root, need(slug, sub)?)
                .map_err(|e| format!("cannot read the plugin: {e:?}"))?;
            println!("{d}");
            Ok(())
        }
        "approve" => {
            let id = need(slug, sub)?;
            let want = args
                .get(2)
                .ok_or("plugin approve needs the digest you reviewed")?;
            // One read: the digest and the validated plugin come from the same bytes.
            let (found, bytes) = ce_plugin::snapshot(&root, id)
                .map_err(|e| format!("cannot read the plugin: {e:?}"))?;
            if &found != want {
                return Err(format!(
                    "the plugin on disk has digest {found}, not the one you gave; nothing approved"
                ));
            }
            let text = String::from_utf8(bytes).map_err(|_| "the plugin is not UTF-8")?;
            let plugin = ce_plugin::parse(&text, &ce_plugin::builtin_registry(), &engine())
                .map_err(|v| {
                    let mut m = String::from("the plugin is not valid:");
                    for x in v {
                        m.push_str(&format!("\n  {} {}: {}", x.path, x.code, x.message));
                    }
                    m
                })?;
            if plugin.id != id {
                return Err("the folder name differs from plugin.id".into());
            }
            inst.approve(ce_plugin::Pin {
                id: id.to_string(),
                version: plugin.version.clone(),
                digest: found.clone(),
            });
            write_installs(&installs, &inst)?;
            println!("approved {id} {} {found}", plugin.version);
            Ok(())
        }
        "rollback" => {
            let id = need(slug, sub)?;
            let pin = inst.rollback(id).ok_or("nothing to roll back to")?.clone();
            write_installs(&installs, &inst)?;
            println!("{id} is back to {} {}", pin.version, pin.digest);
            Ok(())
        }
        "disable" | "enable" => {
            let id = need(slug, sub)?;
            if sub == "disable" {
                inst.disable(id);
            } else {
                inst.enable(id);
            }
            write_installs(&installs, &inst)?;
            println!("{sub}d {id}");
            Ok(())
        }
        _ => Err("use plugin status | digest | approve | rollback | disable | enable".into()),
    }
}

/// The manifest to serve: `CE_MANIFEST` as written, or `CE_TEMPLATE` composed with a module and,
/// when `CE_PLUGIN` is set, that plugin as approved in `CE_INSTALLS`. Anything unapproved,
/// edited since approval, or invalid is an error, never a quiet fall back to the defaults.
fn load_manifest() -> Result<ce_manifest::Manifest, String> {
    let template = std::env::var("CE_TEMPLATE").ok().filter(|s| !s.is_empty());
    let Some(tpath) = template else {
        let path = var("CE_MANIFEST")?;
        let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
        return ce_manifest::validate(&bytes).map_err(|issues| {
            let mut msg = format!("{path} is not a valid manifest:");
            for i in issues {
                msg.push_str(&format!("\n  {} {}: {}", i.path, i.code, i.message));
            }
            msg
        });
    };
    if std::env::var("CE_MANIFEST").is_ok_and(|s| !s.is_empty()) {
        return Err("set CE_MANIFEST or CE_TEMPLATE, not both".into());
    }
    let module = var("CE_MODULE")?;
    let text = std::fs::read_to_string(&tpath).map_err(|e| format!("cannot read {tpath}: {e}"))?;
    let template: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{tpath} is not JSON: {e}"))?;
    let plugin = std::env::var("CE_PLUGIN").ok().filter(|s| !s.is_empty());
    let (root, installed) = match &plugin {
        Some(_) => (
            std::path::PathBuf::from(var("CE_PLUGIN_ROOT")?),
            read_installs(&var("CE_INSTALLS")?)?,
        ),
        None => (std::path::PathBuf::new(), ce_plugin::Installed::new()),
    };
    let composed = ce_plugin::resolve(
        &template,
        &module,
        plugin.as_deref(),
        &root,
        &installed,
        &engine(),
    )
    .map_err(|e| match e {
        ce_plugin::ResolveError::NotApproved(id) => {
            format!("plugin {id} is not approved (or is disabled); run `ce-serve plugin approve`")
        }
        other => format!("cannot start with the plugin: {other:?}"),
    })?;
    ce_manifest::validate_composed(
        serde_json::to_string(&composed.manifest)
            .expect("serialises")
            .as_bytes(),
    )
    .map_err(|_| "the composed manifest failed validation".to_string())
}

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> Result<T, String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v.parse().map_err(|_| format!("{name} must be a number")),
        _ => Ok(default),
    }
}

/// Staff accounts in the installation's schema, as the login's directory.
struct PgDirectory(ce_store_pg::PgStaff);

fn dir_err(e: ce_store_pg::StaffError) -> ce_server::DirectoryFailure {
    match e {
        ce_store_pg::StaffError::Missing => ce_server::DirectoryFailure::Missing,
        ce_store_pg::StaffError::Exists => ce_server::DirectoryFailure::Exists,
        ce_store_pg::StaffError::Backend => ce_server::DirectoryFailure::Backend,
    }
}

fn record_of(r: ce_store_pg::StaffRow) -> ce_server::StaffRecord {
    ce_server::StaffRecord {
        username: r.username,
        hash: r.hash,
        roles: r.roles,
        active: r.active,
        must_rotate: r.must_rotate,
        pin_updated_at: r.pin_updated_at,
    }
}

impl ce_server::StaffDirectory for PgDirectory {
    fn get(&self, u: &str) -> Result<Option<ce_server::StaffRecord>, ce_server::DirectoryFailure> {
        self.0.get(u).map(|r| r.map(record_of)).map_err(dir_err)
    }

    fn list(&self) -> Result<Vec<ce_server::StaffRecord>, ce_server::DirectoryFailure> {
        self.0
            .list()
            .map(|v| v.into_iter().map(record_of).collect())
            .map_err(dir_err)
    }

    fn create(
        &self,
        r: ce_server::StaffRecord,
        actor: &str,
    ) -> Result<(), ce_server::DirectoryFailure> {
        self.0
            .create(
                &ce_store_pg::StaffRow {
                    username: r.username,
                    hash: r.hash,
                    roles: r.roles,
                    active: r.active,
                    must_rotate: r.must_rotate,
                    pin_updated_at: r.pin_updated_at,
                },
                actor,
            )
            .map_err(dir_err)
    }

    fn set_pin(
        &self,
        u: &str,
        hash: &str,
        must_rotate: bool,
        now: u64,
        actor: &str,
    ) -> Result<(), ce_server::DirectoryFailure> {
        self.0
            .set_pin(u, hash, must_rotate, now, actor)
            .map_err(dir_err)
    }

    fn set_roles(
        &self,
        u: &str,
        roles: &[String],
        actor: &str,
    ) -> Result<(), ce_server::DirectoryFailure> {
        self.0.set_roles(u, roles, actor).map_err(dir_err)
    }

    fn set_active(
        &self,
        u: &str,
        active: bool,
        actor: &str,
    ) -> Result<(), ce_server::DirectoryFailure> {
        self.0.set_active(u, active, actor).map_err(dir_err)
    }

    fn writable(&self) -> bool {
        true
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The staff PIN login: from `CE_STAFF_FILE` alone (a read-only list), or with
/// `CE_STAFF_SOURCE=postgres` from the installation's database, where `CE_STAFF_FILE` only seeds
/// an empty table. Returns the login, its tenant and the token lifetime.
/// `CE_PUBLIC_WORKSPACE`: the one workspace name that public projection URLs
/// (`/public/{installation}/{workspace}/{projection}`) answer for, mapped to `CE_TENANT`.
/// Unset leaves every public URL answering 404.
fn public_workspace() -> Result<Option<(String, String)>, String> {
    let Some(name) = std::env::var("CE_PUBLIC_WORKSPACE")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let ok = name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        return Err(
            "CE_PUBLIC_WORKSPACE is lowercase letters, digits and hyphens, at most 63".into(),
        );
    }
    let tenant = var("CE_TENANT").map_err(|_| "CE_PUBLIC_WORKSPACE needs CE_TENANT".to_string())?;
    Ok(Some((name, tenant)))
}

fn staff_login(store: &TypedPgStore) -> Result<Option<(ce_server::PinLogin, String, u64)>, String> {
    let file = std::env::var("CE_STAFF_FILE")
        .ok()
        .filter(|s| !s.is_empty());
    let source = std::env::var("CE_STAFF_SOURCE").unwrap_or_default();
    let in_db = match source.as_str() {
        "" | "file" => false,
        "postgres" => true,
        _ => return Err("CE_STAFF_SOURCE must be `postgres`, `file` or unset".into()),
    };
    if file.is_none() && !in_db {
        return Ok(None);
    }
    let tenant = var("CE_TENANT")?;
    let ttl = env_num("CE_TOKEN_TTL_SECS", 12 * 3600u64)?;
    let mut policy = ce_server::PinPolicy {
        max_failed: env_num("CE_LOGIN_MAX_FAILED", 5u32)?,
        lockout_secs: env_num("CE_LOGIN_LOCKOUT_SECS", 900u64)?,
        min_pin_len: env_num("CE_MIN_PIN_LEN", 6usize)?,
        ..Default::default()
    };
    if !(1..=100).contains(&policy.max_failed) || !(1..=86_400).contains(&policy.lockout_secs) {
        return Err("CE_LOGIN_MAX_FAILED is 1 to 100 and CE_LOGIN_LOCKOUT_SECS 1 to 86400".into());
    }
    if !(4..=64).contains(&policy.min_pin_len) {
        return Err("CE_MIN_PIN_LEN is 4 to 64".into());
    }
    if let Ok(l) = std::env::var("CE_ROLE_LADDER") {
        if !l.is_empty() {
            policy.ladder = l.split(',').map(|r| r.trim().to_string()).collect();
        }
    }
    let text = match &file {
        Some(path) => {
            Some(std::fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))?)
        }
        None => None,
    };
    let now: std::sync::Arc<dyn Fn() -> u64 + Send + Sync> = std::sync::Arc::new(now_secs);
    if !in_db {
        let text = text.expect("a file is set when the source is not the database");
        let login = ce_server::PinLogin::from_json(&text, policy, now)
            .map_err(|e| format!("{}: {e}", file.unwrap_or_default()))?;
        return Ok(Some((login, tenant, ttl)));
    }
    let staff = store
        .staff(&tenant)
        .map_err(|e| format!("cannot prepare staff storage: {e:?}"))?;
    if staff.count().map_err(|e| format!("staff table: {e:?}"))? == 0 {
        let Some(text) = &text else {
            return Err(
                "no staff accounts yet: set CE_STAFF_FILE once to add the first ones".into(),
            );
        };
        let records = ce_server::parse_staff_file(text, &policy)
            .map_err(|e| format!("{}: {e}", file.clone().unwrap_or_default()))?;
        if records.is_empty() {
            return Err("the staff file has no users to start with".into());
        }
        for r in records {
            staff
                .create(
                    &ce_store_pg::StaffRow {
                        username: r.username,
                        hash: r.hash,
                        roles: r.roles,
                        active: r.active,
                        must_rotate: r.must_rotate,
                        pin_updated_at: now_secs(),
                    },
                    "seed",
                )
                .map_err(|e| format!("cannot seed staff: {e:?}"))?;
        }
    }
    let login =
        ce_server::PinLogin::with_directory(std::sync::Arc::new(PgDirectory(staff)), policy, now);
    Ok(Some((login, tenant, ttl)))
}

/// Uploaded files in the installation's schema.
struct PgFiles(ce_store_pg::PgBlobs);

impl ce_server::BlobStore for PgFiles {
    fn put(
        &self,
        scope: &ce_auth::TenantScope,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), String> {
        self.0
            .put(scope.tenant_id(), key, bytes, content_type)
            .map_err(|e| format!("{e:?}"))
    }

    fn get(
        &self,
        scope: &ce_auth::TenantScope,
        key: &str,
    ) -> Result<Option<(Vec<u8>, String)>, String> {
        self.0
            .get(scope.tenant_id(), key)
            .map_err(|e| format!("{e:?}"))
    }
}

/// The gateway provider when `CE_PROVIDER_URL` is set. The token is never printed.
fn provider_from_env() -> Result<Option<ce_provider_http::HttpProvider>, String> {
    let Some(url) = std::env::var("CE_PROVIDER_URL")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };
    let token = var("CE_PROVIDER_TOKEN")?;
    let secs = env_num("CE_PROVIDER_TIMEOUT_SECS", 30u64)?;
    if !(1..=300).contains(&secs) {
        return Err("CE_PROVIDER_TIMEOUT_SECS is 1 to 300".into());
    }
    ce_provider_http::HttpProvider::new(&url, &token, std::time::Duration::from_secs(secs))
        .map(Some)
        .map_err(|e| format!("CE_PROVIDER_URL: {e}"))
}

/// `ce-serve staff hash`: reads a PIN from stdin (one line) and prints its hash.
fn staff_command(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("hash") {
        return Err("use staff hash (the PIN is read from stdin)".into());
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read the PIN: {e}"))?;
    let pin = line.trim_end_matches(['\r', '\n']);
    let hash = ce_server::hash_pin_random(pin).ok_or("the PIN is empty")?;
    println!("{hash}");
    Ok(())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("staff") {
        return staff_command(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("plugin") {
        return plugin_command(&args[1..]);
    }
    let cmd = args.first().cloned().unwrap_or_else(|| "serve".into());
    if !matches!(cmd.as_str(), "serve" | "install" | "files" | "migrate") {
        return Err(format!(
            "unknown command `{cmd}` (use `install`, `migrate` or `serve`)"
        ));
    }
    let url = var("CE_DATABASE_URL")?;
    let key = var("CE_JWT_KEY")?;
    let installation = var("CE_INSTALLATION")?;
    let schema = std::env::var("CE_SCHEMA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_schema(&installation));
    let mut pool = ce_store_pg::PoolConfig {
        tls: db_tls()?,
        ..Default::default()
    };
    if let Ok(v) = std::env::var("CE_POOL_SIZE") {
        pool.max = v
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=256).contains(n))
            .ok_or("CE_POOL_SIZE must be a number from 1 to 256")?;
    }
    let bind = std::env::var("CE_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let manifest = load_manifest()?;
    let verifier =
        Verifier::new(key.as_bytes()).map_err(|_| "CE_JWT_KEY is too short".to_string())?;

    // Blocking Postgres work happens here, before and after the async runtime, never inside it.
    if cmd == "files" {
        let hours = match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("sweep"), Some(h)) => h
                .parse::<u64>()
                .ok()
                .filter(|h| *h <= 24 * 365)
                .ok_or("use files sweep <hours>")?,
            _ => return Err("use files sweep <hours>".into()),
        };
        let store = TypedPgStore::open_with(&url, &manifest, &schema, pool)
            .map_err(|e| format!("cannot open schema {schema}: {e:?}"))?;
        let n = store
            .sweep_blobs(hours * 3600)
            .map_err(|e| format!("sweep failed: {e:?}"))?;
        println!("deleted {n} unreferenced file(s)");
        return Ok(());
    }
    if cmd == "migrate" {
        return migrate_command(&args[1..], &url, &manifest, &schema, pool);
    }
    if cmd == "install" {
        let store = TypedPgStore::install_with(&url, &manifest, &schema, pool.clone())
            .map_err(|e| format!("install failed: {e:?}"))?;
        drop(store);
        println!("installed schema {schema}");
        return Ok(());
    }
    if !schema_exists(&url, &pool.tls, &schema)? {
        return Err(format!(
            "schema {schema} does not exist (run `ce-serve install` first)"
        ));
    }
    let store = TypedPgStore::open_with(&url, &manifest, &schema, pool).map_err(|e| {
        format!("cannot open schema {schema}: {e:?} (run `ce-serve install` first)")
    })?;
    let blobs = match std::env::var("CE_FILES")
        .ok()
        .filter(|v| !v.is_empty())
        .as_deref()
    {
        None => None,
        Some("postgres") => Some(
            store
                .blobs()
                .map_err(|e| format!("cannot prepare file storage: {e:?}"))?,
        ),
        Some(_) => return Err("CE_FILES must be `postgres` or unset".into()),
    };
    let provider = provider_from_env()?;
    let login = staff_login(&store)?;
    let mut state = AppState::new(store, verifier, manifest, &installation);
    if let Some(b) = blobs {
        state = state.with_blobs(std::sync::Arc::new(PgFiles(b)));
    }
    if let Some(p) = provider {
        state = state.with_provider(std::sync::Arc::new(p));
    }
    if let Some((login, tenant, ttl)) = login {
        state = state
            .with_login(Box::new(login), &tenant, ttl)
            .map_err(|e| format!("cannot enable sign-in: {e}"))?;
        if let Ok(role) = std::env::var("CE_STAFF_ADMIN_ROLE") {
            if !role.is_empty() {
                state = state.with_staff_admin_role(&role);
            }
        }
    }
    if let Some((name, tenant)) = public_workspace()? {
        if state.verifier.public_scope(&tenant).is_none() {
            return Err("CE_TENANT must be a lowercase uuid".into());
        }
        state = state.with_public_tenants(std::sync::Arc::new(move |w: &str| {
            (w == name).then(|| tenant.clone())
        }));
    }
    let app = router(state);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    let app = rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .map_err(|e| format!("cannot listen on {bind}: {e}"))?;
        println!("serving {installation} on {bind}");
        axum::serve(listener, app.clone())
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| format!("server error: {e}"))?;
        Ok::<_, String>(app)
    })?;
    drop(rt);
    drop(app);
    Ok(())
}

/// `ce-serve migrate [--plan] [--adopt]`: applies the migrations this binary carries to an
/// installed schema (forward-only, ordered, checksum-pinned). `--plan` prints what is pending
/// and writes nothing. `--adopt` records the baseline for a schema installed before migration
/// history existed; it is never done implicitly.
fn migrate_command(
    flags: &[String],
    url: &str,
    manifest: &ce_manifest::Manifest,
    schema: &str,
    pool: ce_store_pg::PoolConfig,
) -> Result<(), String> {
    use ce_store_pg::migrations::{embedded, MigrateError};
    let mut plan_only = false;
    let mut adopt = false;
    for f in flags {
        match f.as_str() {
            "--plan" => plan_only = true,
            "--adopt" => adopt = true,
            other => return Err(format!("unknown flag `{other}` (use --plan or --adopt)")),
        }
    }
    if plan_only && adopt {
        return Err("--plan writes nothing, so it cannot be combined with --adopt".into());
    }
    if !schema_exists(url, &pool.tls, schema)? {
        return Err(format!(
            "schema {schema} does not exist (run `ce-serve install` first)"
        ));
    }
    let store = TypedPgStore::open_with(url, manifest, schema, pool)
        .map_err(|e| format!("cannot open schema {schema}: {e:?}"))?;
    let list = embedded();
    let actor = std::env::var("CE_MIGRATE_ACTOR")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ce-serve".into());
    if adopt {
        store
            .adopt_migration_baseline(&list, &actor)
            .map_err(|e| e.to_string())?;
        println!("schema {schema}: baseline (version 1) recorded");
    }
    match store.migration_plan(&list) {
        Ok(plan) if plan_only => {
            println!("schema {schema}: at version {}", plan.current);
            if plan.pending.is_empty() {
                println!("nothing pending");
            }
            for (v, name) in plan.pending {
                println!("pending {v:04} {name}");
            }
            Ok(())
        }
        Ok(_) => {
            let done = store.migrate(&list, &actor).map_err(|e| e.to_string())?;
            if done.pending.is_empty() {
                println!(
                    "schema {schema}: already at version {} (nothing to do)",
                    done.current
                );
            }
            for (v, name) in done.pending {
                println!("applied {v:04} {name}");
            }
            Ok(())
        }
        Err(MigrateError::NoHistory) => Err(MigrateError::NoHistory.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM (what Render and Docker send to stop a service).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("ce-serve: {msg}");
            ExitCode::FAILURE
        }
    }
}
