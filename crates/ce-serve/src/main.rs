//! `ce-serve`: runs one CEngine installation.
//!
//! ```text
//! ce-serve install   create the installation's Postgres schema (fails if it exists)
//! ce-serve [serve]   open the schema and serve the HTTP API (default)
//! ```
//!
//! Configuration is by environment variable; secrets are never printed.
//!
//! | Variable | Meaning |
//! | --- | --- |
//! | `CE_DATABASE_URL` | Postgres connection string (required) |
//! | `CE_MANIFEST` | path to the template manifest JSON (required) |
//! | `CE_JWT_KEY` | HS256 signing key, at least 32 bytes (required) |
//! | `CE_INSTALLATION` | installation id used in URLs, `/v1/{id}/...` (required) |
//! | `CE_SCHEMA` | Postgres schema name (default `ce_` + the installation id) |
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

fn schema_exists(url: &str, schema: &str) -> Result<bool, String> {
    let mut client = postgres::Client::connect(url, postgres::NoTls)
        .map_err(|e| format!("cannot connect to the database: {e}"))?;
    let rows = client
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = $1",
            &[&schema],
        )
        .map_err(|e| format!("cannot check schema {schema}: {e}"))?;
    Ok(!rows.is_empty())
}

fn run() -> Result<(), String> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    if !matches!(cmd.as_str(), "serve" | "install") {
        return Err(format!(
            "unknown command `{cmd}` (use `install` or `serve`)"
        ));
    }
    let url = var("CE_DATABASE_URL")?;
    let path = var("CE_MANIFEST")?;
    let key = var("CE_JWT_KEY")?;
    let installation = var("CE_INSTALLATION")?;
    let schema = std::env::var("CE_SCHEMA")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_schema(&installation));
    let bind = std::env::var("CE_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());

    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let manifest = ce_manifest::validate(&bytes).map_err(|issues| {
        let mut msg = format!("{path} is not a valid manifest:");
        for i in issues {
            msg.push_str(&format!("\n  {} {}: {}", i.path, i.code, i.message));
        }
        msg
    })?;
    let verifier =
        Verifier::new(key.as_bytes()).map_err(|_| "CE_JWT_KEY is too short".to_string())?;

    // Blocking Postgres work happens here, before and after the async runtime, never inside it.
    if cmd == "install" {
        let store = TypedPgStore::install(&url, &manifest, &schema)
            .map_err(|e| format!("install failed: {e:?}"))?;
        drop(store);
        println!("installed schema {schema}");
        return Ok(());
    }
    if !schema_exists(&url, &schema)? {
        return Err(format!(
            "schema {schema} does not exist (run `ce-serve install` first)"
        ));
    }
    let store = TypedPgStore::open(&url, &manifest, &schema).map_err(|e| {
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
