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
//! Staff records come from a [`StaffDirectory`]: the file (read-only) or a database table the host
//! supplies, in which case the same type also answers the staff-administration calls
//! ([`StaffAdmin`]): change one's own PIN, create staff, change roles, switch accounts off, reset
//! a PIN (which forces the person to pick their own).
//!
//! Lockout state lives in this process. One instance is exact; several instances each keep
//! their own count, so run one until the counter moves into the database.

use crate::login::{Identity, Login, LoginError};
use crate::staff::{
    AdminError, DirectoryFailure, MemoryDirectory, StaffAdmin, StaffDirectory, StaffInfo,
    StaffRecord,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

const FORMAT: &str = "kpn-scrypt";
const KEY_BYTES: usize = 32;
/// Most scrypt work one stored hash may demand (log2 N 16 with r 8 is 64 MiB); the memory budget
/// below caps how many such checks run at once.
const MAX_LOG_N: u8 = 16;
const MAX_R: u32 = 8;
const MAX_P: u32 = 4;
/// Most scrypt memory in use at once, across all sign-ins; more answers `Throttled`.
const MEMORY_BUDGET: usize = 128 << 20;
/// Most usernames tracked at once, guesses at made-up names included.
const MAX_TRACKED: usize = 10_000;
const MAX_USERNAME: usize = 120;
const MAX_PIN: usize = 64;

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
    /// Shortest PIN a person or an admin may set (the legacy sign-in used 6).
    pub min_pin_len: usize,
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
            min_pin_len: 6,
        }
    }
}

#[derive(Default)]
struct Track {
    fails: u32,
    locked_until: u64,
}

pub struct PinLogin {
    dir: Arc<dyn StaffDirectory>,
    policy: PinPolicy,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    tracks: Mutex<HashMap<String, Track>>,
    /// Scrypt memory in use now, in bytes.
    in_use: AtomicUsize,
    /// What an unknown username is checked against: the dearest hash in the list (or a built-in one
    /// for a database directory), so both cases cost the same.
    dummy: Parsed,
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
    /// Bytes of memory one check needs: 128 * r * N.
    memory: usize,
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
        memory: 128 * r as usize * n as usize,
    })
}

fn derive(pin: &str, params: &scrypt::Params, salt: &[u8], len: usize) -> Option<Vec<u8>> {
    let mut out = vec![0u8; len];
    scrypt::scrypt(pin.as_bytes(), salt, params, &mut out).ok()?;
    Some(out)
}

/// Makes a hash for `pin` in the stored format, using `salt` (at least 8 bytes).
pub fn hash_pin(pin: &str, salt: &[u8]) -> Option<String> {
    if pin.is_empty() || pin.len() > MAX_PIN || salt.len() < 8 {
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

fn verify(pin: &str, p: &Parsed) -> bool {
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
        let records = parse_staff_file(text, &policy)?;
        let dummy = records
            .iter()
            .filter_map(|r| parse_hash(&r.hash))
            .max_by_key(|p| p.memory)
            .unwrap_or_else(|| parse_hash(DUMMY).expect("the built-in dummy hash parses"));
        let dir = MemoryDirectory::read_only(records);
        Ok(Self::build(Arc::new(dir), policy, now, dummy))
    }

    /// A login over any directory (a database table, for instance). Unknown usernames are checked
    /// against a built-in hash with the legacy cost.
    pub fn with_directory(
        dir: Arc<dyn StaffDirectory>,
        policy: PinPolicy,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        let dummy = parse_hash(DUMMY).expect("the built-in dummy hash parses");
        Self::build(dir, policy, now, dummy)
    }

    fn build(
        dir: Arc<dyn StaffDirectory>,
        policy: PinPolicy,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
        dummy: Parsed,
    ) -> Self {
        PinLogin {
            dir,
            policy,
            now,
            tracks: Mutex::new(HashMap::new()),
            in_use: AtomicUsize::new(0),
            dummy,
        }
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

    fn give_back(&self, key: &str) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = tracks.get_mut(key) {
            t.fails = t.fails.saturating_sub(1);
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

impl PinLogin {
    /// Counts the attempt, checks the PIN against the account (or the dummy hash) and records the
    /// result. Returns the record only for an active account with the right PIN.
    fn check(&self, key: &str, pin: &str) -> Result<StaffRecord, LoginError> {
        self.begin(key)?;
        let entry = match self.dir.get(key) {
            Ok(e) => e.filter(|r| r.active),
            Err(_) => {
                // No PIN was checked: the attempt is not the caller's wrong guess.
                self.give_back(key);
                return Err(LoginError::Unavailable);
            }
        };
        let known = entry.as_ref().and_then(|e| parse_hash(&e.hash));
        let target = known.as_ref().unwrap_or(&self.dummy);
        if self.in_use.fetch_add(target.memory, Ordering::SeqCst) + target.memory > MEMORY_BUDGET {
            self.in_use.fetch_sub(target.memory, Ordering::SeqCst);
            // Not the caller's wrong guess: give the attempt back.
            self.give_back(key);
            return Err(LoginError::Throttled);
        }
        let ok = verify(pin, target);
        self.in_use.fetch_sub(target.memory, Ordering::SeqCst);
        match entry {
            Some(e) if ok && known.is_some() => {
                self.pass(key);
                Ok(e)
            }
            _ => {
                self.fail(key);
                Err(LoginError::Denied)
            }
        }
    }
}

impl Login for PinLogin {
    fn login(&self, c: &Map<String, Value>) -> Result<Identity, LoginError> {
        let name = c.get("username").and_then(Value::as_str).unwrap_or("");
        let pin = c.get("pin").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() || name.len() > MAX_USERNAME || pin.is_empty() || pin.len() > MAX_PIN {
            return Err(LoginError::Denied);
        }
        let key = key_of(name);
        let e = self.check(&key, pin)?;
        Ok(Identity {
            sub: key,
            roles: self.expand(&e.roles),
            must_rotate: e.must_rotate,
        })
    }

    fn admin(&self) -> Option<&dyn StaffAdmin> {
        Some(self)
    }
}

/// Reads a staff file into records (see [`PinLogin::from_json`] for the shape and the checks).
pub fn parse_staff_file(
    text: &str,
    policy: &PinPolicy,
) -> Result<Vec<StaffRecord>, DirectoryError> {
    let v: Value = serde_json::from_str(text).map_err(|_| DirectoryError::NotJson)?;
    let users = v["users"]
        .as_array()
        .ok_or_else(|| DirectoryError::Shape("`users` must be a list".into()))?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::new();
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
        if !seen.insert(name.clone()) {
            return Err(DirectoryError::Shape(format!(
                "users[{i}].username repeats an earlier one"
            )));
        }
        out.push(StaffRecord {
            username: name,
            hash: hash.to_string(),
            roles,
            active: u["active"].as_bool().unwrap_or(true),
            must_rotate: u["must_rotate"].as_bool().unwrap_or(false),
            pin_updated_at: 0,
        });
    }
    Ok(out)
}

/// A PIN that is too short, a single repeated digit, a straight run or one of the common choices.
/// Same list as the legacy backend's `weak_staff_pin`.
pub fn weak_pin(pin: &str, min_len: usize) -> bool {
    if pin.chars().count() < min_len {
        return true;
    }
    const COMMON: [&str; 10] = [
        "0000", "1111", "1234", "2222", "4321", "9999", "000000", "111111", "123456", "654321",
    ];
    if COMMON.contains(&pin) {
        return true;
    }
    let first = pin.chars().next();
    if pin.chars().all(|c| Some(c) == first) {
        return true;
    }
    "01234567890123456789".contains(pin) || "98765432109876543210".contains(pin)
}

fn info(r: &StaffRecord) -> StaffInfo {
    StaffInfo {
        username: r.username.clone(),
        roles: r.roles.clone(),
        active: r.active,
        must_rotate: r.must_rotate,
        pin_updated_at: r.pin_updated_at,
    }
}

fn fail_of(f: DirectoryFailure) -> AdminError {
    match f {
        DirectoryFailure::Missing => AdminError::Missing,
        DirectoryFailure::Exists => AdminError::Exists,
        DirectoryFailure::ReadOnly => AdminError::ReadOnly,
        DirectoryFailure::Backend => AdminError::Unavailable,
    }
}

impl PinLogin {
    fn rank(&self, roles: &[String]) -> Option<usize> {
        roles
            .iter()
            .filter_map(|r| self.policy.ladder.iter().position(|l| l == r))
            .max()
    }

    /// The roles to store: each must be on the ladder and no higher than the actor's top role.
    fn grantable(
        &self,
        actor_roles: &[String],
        roles: &[String],
    ) -> Result<Vec<String>, AdminError> {
        if roles.is_empty() || roles.iter().any(|r| !self.policy.ladder.contains(r)) {
            return Err(AdminError::Invalid(
                "roles must be on the role ladder".into(),
            ));
        }
        let ceiling = self.rank(actor_roles);
        let want = self.rank(roles);
        if want.is_none() || want > ceiling {
            return Err(AdminError::Forbidden(
                "you cannot grant a role above your own".into(),
            ));
        }
        let mut out: Vec<String> = Vec::new();
        for r in roles {
            if !out.contains(r) {
                out.push(r.clone());
            }
        }
        Ok(out)
    }

    fn pin_ok(&self, pin: &str) -> Result<String, AdminError> {
        if pin.len() > MAX_PIN {
            return Err(AdminError::Invalid(format!(
                "a PIN is at most {MAX_PIN} characters"
            )));
        }
        if weak_pin(pin, self.policy.min_pin_len) {
            return Err(AdminError::Invalid(format!(
                "choose a PIN of at least {} characters that is not a repeat, a run or a common choice",
                self.policy.min_pin_len
            )));
        }
        hash_pin_random(pin).ok_or(AdminError::Unavailable)
    }

    /// The target's record, if the actor outranks or equals it (nobody edits someone above them).
    fn manageable(&self, actor_roles: &[String], key: &str) -> Result<StaffRecord, AdminError> {
        let r = self
            .dir
            .get(key)
            .map_err(fail_of)?
            .ok_or(AdminError::Missing)?;
        if self.rank(&r.roles) > self.rank(actor_roles) {
            return Err(AdminError::Forbidden(
                "that account has a role above yours".into(),
            ));
        }
        Ok(r)
    }

    fn refreshed(&self, key: &str) -> Result<StaffInfo, AdminError> {
        self.dir
            .get(key)
            .map_err(fail_of)?
            .map(|r| info(&r))
            .ok_or(AdminError::Missing)
    }
}

impl StaffAdmin for PinLogin {
    fn change_pin(&self, username: &str, current: &str, new: &str) -> Result<(), AdminError> {
        let key = key_of(username);
        if current.is_empty() || current.len() > MAX_PIN {
            return Err(AdminError::WrongPin);
        }
        // The current PIN goes through the same counting and lockout as a sign-in.
        self.check(&key, current).map_err(|e| match e {
            LoginError::Denied => AdminError::WrongPin,
            LoginError::Throttled => AdminError::Throttled,
            LoginError::Unavailable => AdminError::Unavailable,
        })?;
        if new == current {
            return Err(AdminError::Invalid(
                "the new PIN must differ from the current one".into(),
            ));
        }
        let hash = self.pin_ok(new)?;
        self.dir
            .set_pin(&key, &hash, false, (self.now)(), &key)
            .map_err(fail_of)
    }

    fn list(&self) -> Result<Vec<StaffInfo>, AdminError> {
        Ok(self.dir.list().map_err(fail_of)?.iter().map(info).collect())
    }

    fn create(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        roles: &[String],
        pin: &str,
    ) -> Result<StaffInfo, AdminError> {
        let key = key_of(username);
        if key.is_empty()
            || key.len() > MAX_USERNAME
            || key.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(AdminError::Invalid(
                "a username is 1 to 120 characters without spaces".into(),
            ));
        }
        let roles = self.grantable(actor_roles, roles)?;
        let hash = self.pin_ok(pin)?;
        self.dir
            .create(
                StaffRecord {
                    username: key.clone(),
                    hash,
                    roles,
                    active: true,
                    must_rotate: true,
                    pin_updated_at: (self.now)(),
                },
                actor,
            )
            .map_err(fail_of)?;
        self.refreshed(&key)
    }

    fn update(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        roles: Option<&[String]>,
        active: Option<bool>,
    ) -> Result<StaffInfo, AdminError> {
        let key = key_of(username);
        self.manageable(actor_roles, &key)?;
        if key == key_of(actor) {
            return Err(AdminError::Forbidden(
                "you cannot change your own roles or switch yourself off".into(),
            ));
        }
        if roles.is_none() && active.is_none() {
            return Err(AdminError::Invalid("send roles or active".into()));
        }
        let roles = roles.map(|r| self.grantable(actor_roles, r)).transpose()?;
        if let Some(r) = roles {
            self.dir.set_roles(&key, &r, actor).map_err(fail_of)?;
        }
        if let Some(a) = active {
            self.dir.set_active(&key, a, actor).map_err(fail_of)?;
        }
        self.refreshed(&key)
    }

    fn reset_pin(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        pin: &str,
    ) -> Result<StaffInfo, AdminError> {
        let key = key_of(username);
        self.manageable(actor_roles, &key)?;
        let hash = self.pin_ok(pin)?;
        self.dir
            .set_pin(&key, &hash, true, (self.now)(), actor)
            .map_err(fail_of)?;
        // A reset also clears the lock, so the person can sign in with the temporary PIN.
        self.pass(&key);
        self.refreshed(&key)
    }
}
