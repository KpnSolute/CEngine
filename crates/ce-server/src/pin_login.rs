//! A staff PIN login for [`crate::Login`]: usernames and scrypt-hashed PINs from the owner's
//! staff list, with lockout after repeated failures. CEngine keeps no PIN; the list is a file
//! the owner controls, and a hash is all it holds.
//!
//! The hash format is the one MJCC's legacy backend writes, `kpn-scrypt$1$N$r$p$salt$digest`
//! (hex salt and digest), so existing hashes can be carried over unchanged.
//!
//! Rules carried from the legacy sign-in: five wrong PINs lock the username for 15 minutes
//! (a correct PIN does not unlock it early), unknown usernames are counted and cost the same
//! work as known ones, and a role grants every role below it on the ladder.
//!
//! Lockout state lives in this process. One instance is exact; several instances each keep
//! their own count, so run one until the counter moves into the database.

use crate::login::{Identity, Login, LoginError};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

const FORMAT: &str = "kpn-scrypt";
const KEY_BYTES: usize = 32;
/// Most scrypt work a stored hash may demand (log2 N 16 is 64 MB), so a bad list cannot be a
/// denial of service.
const MAX_LOG_N: u8 = 16;
const MAX_R: u32 = 8;
const MAX_P: u32 = 4;
/// Most hash computations at once; more than this answers `Throttled`.
const MAX_INFLIGHT: usize = 8;
/// Most usernames tracked at once, guesses at made-up names included.
const MAX_TRACKED: usize = 10_000;
const MAX_USERNAME: usize = 120;
const MAX_PIN: usize = 64;

#[derive(Debug, Clone)]
struct StaffEntry {
    hash: String,
    roles: Vec<String>,
}

/// Why a staff list was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum DirectoryError {
    NotJson,
    Shape(String),
}

impl std::fmt::Display for DirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectoryError::NotJson => write!(f, "the staff list is not JSON"),
            DirectoryError::Shape(m) => write!(f, "the staff list is not valid: {m}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PinPolicy {
    /// Wrong PINs allowed before the username locks.
    pub max_failed: u32,
    pub lockout_secs: u64,
    /// Role names from lowest to highest; a role also grants all the ones before it.
    pub ladder: Vec<String>,
}

impl Default for PinPolicy {
    fn default() -> Self {
        PinPolicy {
            max_failed: 5,
            lockout_secs: 15 * 60,
            ladder: ["staff", "assistant", "manager", "admin", "sudo"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Default)]
struct Track {
    fails: u32,
    locked_until: u64,
}

pub struct PinLogin {
    staff: HashMap<String, StaffEntry>,
    policy: PinPolicy,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    tracks: Mutex<HashMap<String, Track>>,
    inflight: AtomicUsize,
}

fn key_of(username: &str) -> String {
    username.trim().to_lowercase()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
        .collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Parsed {
    params: scrypt::Params,
    salt: Vec<u8>,
    digest: Vec<u8>,
}

fn parse_hash(encoded: &str) -> Option<Parsed> {
    let p: Vec<&str> = encoded.split('$').collect();
    if p.len() != 7 || p[0] != FORMAT || p[1] != "1" {
        return None;
    }
    let n: u64 = p[2].parse().ok()?;
    let r: u32 = p[3].parse().ok()?;
    let par: u32 = p[4].parse().ok()?;
    if n < 2 || !n.is_power_of_two() || r == 0 || r > MAX_R || par == 0 || par > MAX_P {
        return None;
    }
    let log_n = n.trailing_zeros() as u8;
    if log_n > MAX_LOG_N {
        return None;
    }
    let salt = from_hex(p[5])?;
    let digest = from_hex(p[6])?;
    if salt.is_empty() || digest.len() != KEY_BYTES {
        return None;
    }
    let params = scrypt::Params::new(log_n, r, par, digest.len()).ok()?;
    Some(Parsed {
        params,
        salt,
        digest,
    })
}

fn derive(pin: &str, params: &scrypt::Params, salt: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; len];
    scrypt::scrypt(pin.as_bytes(), salt, params, &mut out).ok()?;
    Some(out)
}

/// Makes a hash for `pin` in the stored format, using `salt` (at least 8 bytes).
pub fn hash_pin(pin: &str, salt: &[u8]) -> Option<String> {
    if pin.is_empty() || salt.len() < 8 {
        return None;
    }
    let params = scrypt::Params::new(14, 8, 1, KEY_BYTES).ok()?;
    let digest = derive(pin, &params, salt, KEY_BYTES)?;
    Some(format!(
        "{FORMAT}$1$16384$8$1${}${}",
        to_hex(salt),
        to_hex(&digest)
    ))
}

/// A hash for `pin` with a fresh random salt.
pub fn hash_pin_random(pin: &str) -> Option<String> {
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).ok()?;
    hash_pin(pin, &salt)
}

fn verify(pin: &str, encoded: &str) -> bool {
    let Some(p) = parse_hash(encoded) else {
        return false;
    };
    match derive(pin, &p.params, &p.salt, p.digest.len()) {
        Some(got) => got.ct_eq(&p.digest).into(),
        None => false,
    }
}

impl PinLogin {
    /// Reads `{"users":[{"username","pin_hash","roles":[...]}]}`. Every hash must parse, every
    /// username be unique (ignoring case) and every role be on the policy's ladder.
    pub fn from_json(
        text: &str,
        policy: PinPolicy,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, DirectoryError> {
        let v: Value = serde_json::from_str(text).map_err(|_| DirectoryError::NotJson)?;
        let users = v["users"]
            .as_array()
            .ok_or_else(|| DirectoryError::Shape("`users` must be a list".into()))?;
        let mut staff = HashMap::new();
        for (i, u) in users.iter().enumerate() {
            let name = u["username"]
                .as_str()
                .map(key_of)
                .filter(|n| !n.is_empty() && n.len() <= MAX_USERNAME)
                .ok_or_else(|| DirectoryError::Shape(format!("users[{i}].username is missing")))?;
            let hash = u["pin_hash"]
                .as_str()
                .filter(|h| parse_hash(h).is_some())
                .ok_or_else(|| {
                    DirectoryError::Shape(format!("users[{i}].pin_hash is not a kpn-scrypt hash"))
                })?;
            let roles: Vec<String> = u["roles"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if roles.is_empty() || roles.iter().any(|r| !policy.ladder.contains(r)) {
                return Err(DirectoryError::Shape(format!(
                    "users[{i}].roles must be on the role ladder"
                )));
            }
            if staff
                .insert(
                    name,
                    StaffEntry {
                        hash: hash.to_string(),
                        roles,
                    },
                )
                .is_some()
            {
                return Err(DirectoryError::Shape(format!(
                    "users[{i}].username repeats an earlier one"
                )));
            }
        }
        Ok(PinLogin {
            staff,
            policy,
            now,
            tracks: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
        })
    }

    /// Roles for a user: each role they hold plus every lower one, lowest first.
    fn expand(&self, held: &[String]) -> Vec<String> {
        let top = held
            .iter()
            .filter_map(|r| self.policy.ladder.iter().position(|l| l == r))
            .max();
        match top {
            Some(t) => self.policy.ladder[..=t].to_vec(),
            None => Vec::new(),
        }
    }

    /// Counts an attempt before the PIN is checked, so parallel guesses cannot beat the limit.
    fn begin(&self, key: &str) -> Result<(), LoginError> {
        let now = (self.now)();
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        if tracks.len() >= MAX_TRACKED && !tracks.contains_key(key) {
            tracks.retain(|_, t| t.locked_until > now);
            if tracks.len() >= MAX_TRACKED {
                return Err(LoginError::Throttled);
            }
        }
        let t = tracks.entry(key.to_string()).or_default();
        if t.locked_until > now {
            return Err(LoginError::Throttled);
        }
        if t.locked_until != 0 {
            *t = Track::default();
        }
        if t.fails >= self.policy.max_failed {
            t.locked_until = now + self.policy.lockout_secs;
            return Err(LoginError::Throttled);
        }
        t.fails += 1;
        Ok(())
    }

    fn fail(&self, key: &str) {
        let now = (self.now)();
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = tracks.get_mut(key) {
            if t.fails >= self.policy.max_failed && t.locked_until <= now {
                t.locked_until = now + self.policy.lockout_secs;
            }
        }
    }

    fn pass(&self, key: &str) {
        self.tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }
}

/// A made-up hash, checked when the username is unknown so that both cases cost the same.
const DUMMY: &str = "kpn-scrypt$1$16384$8$1$0011223344556677$0000000000000000000000000000000000000000000000000000000000000000";

impl Login for PinLogin {
    fn login(&self, c: &Map<String, Value>) -> Result<Identity, LoginError> {
        let name = c.get("username").and_then(Value::as_str).unwrap_or("");
        let pin = c.get("pin").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() || name.len() > MAX_USERNAME || pin.is_empty() || pin.len() > MAX_PIN {
            return Err(LoginError::Denied);
        }
        let key = key_of(name);
        self.begin(&key)?;
        if self.inflight.fetch_add(1, Ordering::SeqCst) >= MAX_INFLIGHT {
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            // Not the caller's wrong guess: give the attempt back.
            let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = tracks.get_mut(&key) {
                t.fails = t.fails.saturating_sub(1);
            }
            return Err(LoginError::Throttled);
        }
        let entry = self.staff.get(&key);
        let ok = verify(pin, entry.map_or(DUMMY, |e| e.hash.as_str()));
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        match (ok, entry) {
            (true, Some(e)) => {
                self.pass(&key);
                Ok(Identity {
                    sub: key,
                    roles: self.expand(&e.roles),
                })
            }
            _ => {
                self.fail(&key);
                Err(LoginError::Denied)
            }
        }
    }
}
