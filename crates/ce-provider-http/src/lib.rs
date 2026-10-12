//! A [`Provider`] that forwards each request to one HTTPS endpoint, the host's gateway (Infajn).
//! CEngine still never calls a provider or holds a provider key: the gateway does, and this client
//! holds only the gateway's bearer token (an environment secret on the host).
//!
//! Wire format (draft v1): `POST {url}` with `Authorization: Bearer {token}` and a JSON body
//!
//! ```json
//! {"tenant": "<uuid>", "connector": "ai", "operation": "records.extract",
//!  "access": "read", "params": {}, "idempotency_key": null, "approval": null}
//! ```
//!
//! `200` answers `{"data": <value>}`. `503`, a refused connection and a timeout are
//! [`ProviderError::Unavailable`] (retryable). Any other failure is [`ProviderError::Failed`] with a
//! fixed message: the gateway's own error text is never read. Redirects are not followed, a reply over 1 MiB is
//! refused, and the URL must be `https` (plain `http` only for a loopback address).

use ce_auth::TenantScope;
use ce_provider::{Access, Provider, ProviderError, ProviderRequest, ProviderResponse};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

const MAX_REPLY: u64 = 1 << 20;

pub struct HttpProvider {
    agent: ureq::Agent,
    url: String,
    token: String,
}

impl HttpProvider {
    /// `timeout` bounds the whole call. Fails on a URL that is not https (or loopback http) and on
    /// an empty token.
    pub fn new(url: &str, token: &str, timeout: Duration) -> Result<HttpProvider, String> {
        if token.is_empty() {
            return Err("the gateway token is empty".into());
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or("the gateway URL has no scheme")?;
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        if host.contains('@') {
            return Err("the gateway URL must not contain credentials".into());
        }
        let name = if let Some(v6) = host.strip_prefix('[') {
            v6.split(']').next().unwrap_or("")
        } else {
            host.split(':').next().unwrap_or("")
        };
        let loopback = matches!(name, "localhost" | "127.0.0.1" | "::1");
        match scheme {
            "https" => {}
            "http" if loopback => {}
            _ => return Err("the gateway URL must be https (http only for localhost)".into()),
        }
        if name.is_empty() {
            return Err("the gateway URL has no host".into());
        }
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(timeout)
            .build();
        Ok(HttpProvider {
            agent,
            url: url.to_string(),
            token: token.to_string(),
        })
    }
}

fn fixed() -> ProviderError {
    ProviderError::Failed("the provider could not complete the request".into())
}

impl Provider for HttpProvider {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let body = json!({
            "tenant": scope.tenant_id(),
            "connector": req.connector.name(),
            "operation": req.operation,
            "access": if req.access == Access::Write { "write" } else { "read" },
            "params": req.params,
            "idempotency_key": req.idempotency_key,
            "approval": req.approval,
        });
        let sent = self
            .agent
            .post(&self.url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .send_json(body);
        match sent {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader()
                    .take(MAX_REPLY + 1)
                    .read_to_end(&mut buf)
                    .map_err(|_| ProviderError::Unavailable)?;
                if buf.len() as u64 > MAX_REPLY {
                    return Err(fixed());
                }
                let v: Value = serde_json::from_slice(&buf).map_err(|_| fixed())?;
                match v.get("data") {
                    Some(d) => Ok(ProviderResponse { data: d.clone() }),
                    None => Err(fixed()),
                }
            }
            Err(ureq::Error::Status(503, _)) | Err(ureq::Error::Transport(_)) => {
                Err(ProviderError::Unavailable)
            }
            Err(_) => Err(fixed()),
        }
    }
}
