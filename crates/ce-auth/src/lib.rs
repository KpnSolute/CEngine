//! CEngine auth (draft v1). Identity comes from Platform; CEngine only verifies it.
//!
//! A token is a compact JWT signed with HMAC-SHA256 (`alg` must be `HS256`; `none` and every
//! other algorithm are rejected). A valid token yields a [`Principal`] whose [`TenantScope`]
//! is the only way to obtain a scope outside tests, so no query can run without one.
//!
//! Claims: `sub` (user id), `tenant` (uuid), `roles` (list), `exp` and optional `nbf`
//! (unix seconds). The caller passes `now`, so verification is deterministic and testable.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Proof that a request is scoped to one tenant. Only this crate can build one.
#[derive(Debug, Clone)]
pub struct TenantScope {
    tenant_id: String,
}

impl TenantScope {
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub roles: Vec<String>,
    /// Seconds since the epoch when the token was signed (0 when the token has no `iat`).
    pub issued_at: u64,
    scope: TenantScope,
}

impl Principal {
    pub fn scope(&self) -> &TenantScope {
        &self.scope
    }
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    BadAlgorithm,
    BadSignature,
    Expired,
    NotYetValid,
    BadClaims,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    tenant: String,
    #[serde(default)]
    roles: Vec<String>,
    exp: u64,
    nbf: Option<u64>,
    iat: Option<u64>,
}

pub struct Verifier {
    key: Vec<u8>,
}

impl Verifier {
    /// Keys under 32 bytes are refused: a short shared secret is brute-forceable.
    pub fn new(key: &[u8]) -> Result<Self, AuthError> {
        if key.len() < 32 {
            return Err(AuthError::BadClaims);
        }
        Ok(Verifier { key: key.to_vec() })
    }

    /// A scope for a public, unauthenticated read of one tenant's data. The tenant must come
    /// from the host's own lookup (a workspace name resolved through its directory), never from
    /// anything the caller sent as an identity, and only a manifest-declared public projection
    /// may use it. Holding a `Verifier` is the proof that the host asked for this.
    pub fn public_scope(&self, tenant_id: &str) -> Option<TenantScope> {
        is_uuid(tenant_id).then(|| TenantScope {
            tenant_id: tenant_id.to_string(),
        })
    }

    /// Signs a token for `sub` in `tenant` with `roles`, valid from `now` for `ttl_secs` (1 second
    /// to 24 hours). The host decides who may ask: this only turns an identity it has already
    /// checked into the token `verify_token` accepts. Fails on an empty `sub` or a tenant that is
    /// not a canonical lowercase uuid.
    pub fn sign_token(
        &self,
        sub: &str,
        tenant: &str,
        roles: &[String],
        now: u64,
        ttl_secs: u64,
    ) -> Result<String, AuthError> {
        if sub.is_empty() || !is_uuid(tenant) || !(1..=86_400).contains(&ttl_secs) {
            return Err(AuthError::BadClaims);
        }
        let h = B64.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let p = B64.encode(
            serde_json::json!({
                "sub": sub, "tenant": tenant, "roles": roles,
                "iat": now, "exp": now.saturating_add(ttl_secs),
            })
            .to_string(),
        );
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).map_err(|_| AuthError::BadClaims)?;
        mac.update(format!("{h}.{p}").as_bytes());
        Ok(format!(
            "{h}.{p}.{}",
            B64.encode(mac.finalize().into_bytes())
        ))
    }

    pub fn verify_token(&self, token: &str, now: u64) -> Result<Principal, AuthError> {
        let mut parts = token.split('.');
        let (h, p, s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return Err(AuthError::Malformed),
        };
        let header: Header = decode(h)?;
        if header.alg != "HS256" {
            return Err(AuthError::BadAlgorithm);
        }
        let sig = B64.decode(s).map_err(|_| AuthError::Malformed)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).map_err(|_| AuthError::BadClaims)?;
        mac.update(h.as_bytes());
        mac.update(b".");
        mac.update(p.as_bytes());
        let expected = mac.finalize().into_bytes();
        if !bool::from(expected.as_slice().ct_eq(&sig)) {
            return Err(AuthError::BadSignature);
        }
        let c: Claims = decode(p)?;
        if now >= c.exp {
            return Err(AuthError::Expired);
        }
        if c.nbf.is_some_and(|nbf| now < nbf) {
            return Err(AuthError::NotYetValid);
        }
        if !is_uuid(&c.tenant) || c.sub.is_empty() {
            return Err(AuthError::BadClaims);
        }
        Ok(Principal {
            user_id: c.sub,
            roles: c.roles,
            issued_at: c.iat.unwrap_or(0),
            scope: TenantScope {
                tenant_id: c.tenant,
            },
        })
    }
}

fn decode<T: for<'a> Deserialize<'a>>(part: &str) -> Result<T, AuthError> {
    let bytes = B64.decode(part).map_err(|_| AuthError::Malformed)?;
    serde_json::from_slice(&bytes).map_err(|_| AuthError::Malformed)
}

/// Canonical lowercase 8-4-4-4-12 hex; the planner binds it with a `::uuid` cast.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

#[cfg(test)]
mod issue_tests {
    use super::*;
    const T: &str = "11111111-1111-1111-1111-111111111111";

    #[test]
    fn a_signed_token_verifies_and_expires() {
        let v = Verifier::new(b"0123456789abcdef0123456789abcdef").unwrap();
        let t = v
            .sign_token("u1", T, &["owner".to_string()], 1000, 60)
            .unwrap();
        let p = v.verify_token(&t, 1059).unwrap();
        assert_eq!((p.user_id.as_str(), p.scope().tenant_id()), ("u1", T));
        assert_eq!(p.roles, ["owner"]);
        assert_eq!(v.verify_token(&t, 1060).err(), Some(AuthError::Expired));
    }

    #[test]
    fn bad_inputs_are_refused() {
        let v = Verifier::new(b"0123456789abcdef0123456789abcdef").unwrap();
        for (sub, tenant, ttl) in [
            ("", T, 60),
            ("u", "nope", 60),
            ("u", T, 0),
            ("u", T, 86_401),
        ] {
            assert_eq!(
                v.sign_token(sub, tenant, &[], 1000, ttl),
                Err(AuthError::BadClaims)
            );
        }
    }
}
