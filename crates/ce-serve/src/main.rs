//! `ce-serve`: runs one CEngine installation.
//!
//! ```text
//! ce-serve install   create the installation's Postgres schema (fails if it exists)
//! ce-serve [serve]   open the schema and serve the HTTP API (default)
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
    match std::fs::read_to_string(path) {
        Ok(text) => ce_plugin::Installed::from_json(&text)
            .map_err(|e| format!("{path} is not a valid install record: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ce_plugin::Installed::new()),
        Err(e) => Err(format!("cannot read {path}: {e}")),
    }
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
            let found = ce_plugin::digest_of(&root, id)
                .map_err(|e| format!("cannot read the plugin: {e:?}"))?;
            if &found != want {
                return Err(format!(
                    "the plugin on disk has digest {found}, not the one you gave; nothing approved"
                ));
            }
            let text = std::fs::read_to_string(
                root.join(ce_plugin::PLUGINS_DIR)
                    .join(id)
                    .join(ce_plugin::DESCRIPTOR),
            )
            .map_err(|e| format!("cannot read the plugin: {e}"))?;
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

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("plugin") {
        return plugin_command(&args[1..]);
    }
    let cmd = args.first().cloned().unwrap_or_else(|| "serve".into());
    if !matches!(cmd.as_str(), "serve" | "install") {
        return Err(format!(
            "unknown command `{cmd}` (use `install` or `serve`)"
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
    let app = router(AppState::new(store, verifier, manifest, &installation));

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
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| format!("server error: {e}"))?;
        Ok::<_, String>(app)
    })?;
    drop(rt);
    drop(app);
    Ok(())
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
