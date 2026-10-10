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

    /// Test helper behind the `test-util` feature.
    #[cfg(feature = "test-util")]
    pub fn for_tests(tenant_id: impl Into<String>) -> Self {
        TenantScope {
            tenant_id: tenant_id.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Principal {
    pub user_id: String,
    pub roles: Vec<String>,
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
