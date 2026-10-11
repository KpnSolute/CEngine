//! TLS for database connections (rustls, no system libraries).
//!
//! With TLS on, a connection must be encrypted (the server refusing TLS is an error, never a
//! silent fall back to plain text) and the server's certificate must be valid for the host in
//! the connection URL. That is stricter than libpq's `sslmode=require` and matches
//! `verify-full`. The URL's own `sslmode` is overridden when TLS is on.

use ce_store::StoreError;
use postgres::{config::SslMode, Client, Config, NoTls};
use std::sync::Arc;

/// How connections to the database are secured.
#[derive(Clone, Debug, Default)]
pub enum Tls {
    /// Plain text. Only for a database on the same machine or a private socket.
    #[default]
    Off,
    /// TLS, trusting the public web roots.
    WebPki,
    /// TLS, trusting only the CA certificate(s) in this PEM (for a private CA or a managed
    /// database that publishes its own).
    Ca(Vec<u8>),
}

/// A ready way to open connections; building it validates the TLS settings once.
#[derive(Clone)]
pub(crate) enum Connector {
    Plain,
    Tls(tokio_postgres_rustls::MakeRustlsConnect),
}

fn bad(msg: &str) -> StoreError {
    StoreError::Backend(msg.to_string())
}

impl Connector {
    pub(crate) fn new(tls: &Tls) -> Result<Connector, StoreError> {
        let mut roots = rustls::RootCertStore::empty();
        match tls {
            Tls::Off => return Ok(Connector::Plain),
            Tls::WebPki => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            Tls::Ca(pem) => {
                let mut rd = pem.as_slice();
                let mut n = 0;
                for cert in rustls_pemfile::certs(&mut rd) {
                    let cert = cert.map_err(|_| bad("the CA file is not valid PEM"))?;
                    roots
                        .add(cert)
                        .map_err(|_| bad("the CA file has an invalid certificate"))?;
                    n += 1;
                }
                if n == 0 {
                    return Err(bad("the CA file has no certificate"));
                }
            }
        }
        let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| bad("TLS setup failed"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Connector::Tls(
            tokio_postgres_rustls::MakeRustlsConnect::new(cfg),
        ))
    }

    pub(crate) fn connect(&self, url: &str) -> Result<Client, postgres::Error> {
        match self {
            Connector::Plain => Client::connect(url, NoTls),
            Connector::Tls(t) => {
                let mut cfg: Config = without_sslmode(url).parse()?;
                cfg.ssl_mode(SslMode::Require);
                cfg.connect(t.clone())
            }
        }
    }
}

/// The connection string without its `sslmode`, which libpq accepts in more spellings
/// (`verify-full`, `verify-ca`...) than the driver parses; TLS-on forces its own mode.
fn without_sslmode(url: &str) -> String {
    let is_sslmode = |kv: &str| kv.split('=').next() == Some("sslmode");
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let Some((base, query)) = url.split_once('?') else {
            return url.to_string();
        };
        let keep: Vec<&str> = query.split('&').filter(|kv| !is_sslmode(kv)).collect();
        if keep.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", keep.join("&"))
        }
    } else {
        url.split_whitespace()
            .filter(|kv| !is_sslmode(kv))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Opens one connection the way the store would. For tools that need the same settings.
pub fn connect(url: &str, tls: &Tls) -> Result<Client, StoreError> {
    Connector::new(tls)?
        .connect(url)
        .map_err(|e| StoreError::Backend(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::without_sslmode;

    #[test]
    fn sslmode_is_dropped_from_urls_and_key_value_strings() {
        assert_eq!(
            without_sslmode("postgresql://u:p@h/db?sslmode=verify-full"),
            "postgresql://u:p@h/db"
        );
        assert_eq!(
            without_sslmode(
                "postgres://h/db?connect_timeout=5&sslmode=verify-ca&application_name=x"
            ),
            "postgres://h/db?connect_timeout=5&application_name=x"
        );
        assert_eq!(without_sslmode("postgres://h/db"), "postgres://h/db");
        assert_eq!(
            without_sslmode("host=h sslmode=verify-full user=u"),
            "host=h user=u"
        );
    }
}
