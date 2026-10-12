//! Token issuing (`POST .../_auth/token`): the seam where a host's own login becomes a CEngine token.
//!
//! CEngine never stores passwords or PINs. The host supplies a [`Login`] that checks whatever
//! credentials it accepts (a Supabase session, a staff PIN, an SSO assertion) and names the
//! caller and their roles; the server then signs a short-lived token for the one tenant the host
//! configured, keeping only roles the manifest defines. The caller never chooses the tenant.

use serde_json::{Map, Value};

/// The only role of a token issued for a forced PIN change.
pub(crate) const ROTATE_ROLE: &str = "_rotate_pin";

/// Who the host's login says the caller is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub sub: String,
    pub roles: Vec<String>,
    /// The person must set a new PIN before doing anything else: the token carries no roles, only
    /// the right to call `POST .../_auth/pin`.
    pub must_rotate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    /// Wrong or unknown credentials. The caller is never told which part.
    Denied,
    /// Too many attempts; the host's rate limit says to wait.
    Throttled,
    /// The host's identity service is down.
    Unavailable,
}

/// Checks credentials. Called off the async runtime, so it may block. Rate limiting and lockout
/// belong here: the server cannot tell a PIN guess from a first try.
pub trait Login: Send + Sync {
    fn login(&self, credentials: &Map<String, Value>) -> Result<Identity, LoginError>;

    /// The staff-administration calls, when this login keeps staff records it can change.
    fn admin(&self) -> Option<&dyn crate::staff::StaffAdmin> {
        None
    }
}

/// The login a server was given, the tenant it issues for and how long tokens last.
pub struct Issuing {
    pub login: Box<dyn Login>,
    pub tenant: String,
    pub ttl_secs: u64,
}

/// Roles the manifest defines, in the order the host gave them, without repeats.
pub(crate) fn known_roles(m: &ce_manifest::Manifest, roles: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in roles {
        if m.value["roles"][r.as_str()].is_object() && !out.contains(r) {
            out.push(r.clone());
        }
    }
    out
}
