//! Guest accounts: people outside the staff, with an email, a password and nothing else.
//!
//! A guest is not staff. Their token carries one role, the manifest's guest role (which the
//! template gives no entity access), so a guest token opens no staff route. What a guest may do is
//! only what the manifest's `guest.resources` declares (see `guest_resources`), and only to their own
//! rows. Passwords are scrypt-hashed in the same `kpn-scrypt` format as staff PINs; the engine
//! stores no password and logs none. Sign-in, sign-up and password reset are rate limited, an
//! unknown email costs the same work as a known one, and a wrong credential is always the same 401.
//!
//! Sessions end on sign-out and on a password reset: [`GuestService::not_before`] answers from
//! memory (warmed from the directory at start), because it runs on every request.

use crate::pin_login::{check_secret, hash_pin_random};
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One guest account. `email` is the lower-cased, trimmed key.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestRecord {
    pub id: String,
    pub email: String,
    pub name: String,
    pub hash: String,
    pub active: bool,
    /// What the guest told us at onboarding (department, extension...), keys the manifest declares.
    pub profile: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestDirError {
    Exists,
    Missing,
    Backend,
}

/// Where guest accounts live. Called off the async runtime, so implementations may block.
pub trait GuestDirectory: Send + Sync {
    fn create(
        &self,
        email: &str,
        name: &str,
        hash: &str,
        profile: &Map<String, Value>,
        now: u64,
    ) -> Result<GuestRecord, GuestDirError>;
    /// Replaces the guest's name and profile.
    fn set_profile(
        &self,
        id: &str,
        name: &str,
        profile: &Map<String, Value>,
    ) -> Result<(), GuestDirError>;
    fn by_email(&self, email: &str) -> Result<Option<GuestRecord>, GuestDirError>;
    fn by_id(&self, id: &str) -> Result<Option<GuestRecord>, GuestDirError>;
    /// Sets a new hash and ends every earlier session of this guest.
    fn set_password(&self, id: &str, hash: &str, now: u64) -> Result<(), GuestDirError>;
    /// Ends every session of this guest issued at or before `now`.
    fn end_sessions(&self, id: &str, now: u64) -> Result<(), GuestDirError>;
    /// Guests whose sessions were ended at or after `since`: the only ones whose live tokens a
    /// cutoff can still affect.
    fn recently_ended(&self, since: u64) -> Result<Vec<(String, u64)>, GuestDirError>;
    /// Keeps a reset token's hash (never the token) until `expires`.
    fn put_reset(&self, id: &str, token_hash: &str, expires: u64) -> Result<(), GuestDirError>;
    /// Uses a reset token once: returns the guest it belongs to, or None when unknown, used or expired.
    fn take_reset(&self, token_hash: &str, now: u64) -> Result<Option<String>, GuestDirError>;
}

/// Sends the password-reset email. The engine holds no mail credentials; the host supplies this.
/// The mail could not be sent. The reason stays with the host; callers never learn it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailError;

pub trait Mailer: Send + Sync {
    fn send_reset(&self, to: &str, name: &str, link: &str) -> Result<(), MailError>;
}

#[derive(Debug, Clone)]
pub struct GuestPolicy {
    pub min_password: usize,
    pub max_password: usize,
    pub max_name: usize,
    /// Token lifetime in seconds.
    pub ttl_secs: u64,
    pub reset_ttl_secs: u64,
    /// Wrong passwords allowed per email per window.
    pub login_per_email: usize,
    pub login_per_ip: usize,
    pub signup_per_ip: usize,
    pub reset_per_email: usize,
    pub reset_per_ip: usize,
    pub window_secs: u64,
    pub signup_window_secs: u64,
    /// Base of the link in the reset email; the token is appended as `?token=`.
    pub reset_link_base: String,
    /// The profile questions, in order. Empty means a guest has only a name and an email.
    pub profile: Vec<ProfileField>,
}

impl Default for GuestPolicy {
    fn default() -> Self {
        GuestPolicy {
            min_password: 8,
            max_password: 64,
            max_name: 80,
            ttl_secs: 12 * 3600,
            reset_ttl_secs: 3600,
            login_per_email: 8,
            login_per_ip: 40,
            signup_per_ip: 10,
            reset_per_email: 3,
            reset_per_ip: 10,
            window_secs: 15 * 60,
            signup_window_secs: 3600,
            reset_link_base: String::new(),
            profile: Vec::new(),
        }
    }
}

/// A signed-in guest, before the server signs the token.
#[derive(Debug, Clone, PartialEq)]
pub struct GuestIdentity {
    pub sub: String,
    pub name: String,
    pub email: String,
    pub profile: Map<String, Value>,
}

/// One question onboarding asks (declared in the manifest's `guest.profile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileField {
    pub key: String,
    pub label: String,
    pub required: bool,
    pub max_length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestError {
    /// A field is unusable; the string says why in words a person can act on.
    Invalid(&'static str, String),
    Exists,
    Denied,
    Throttled,
    Unavailable,
}

/// The subject prefix of every guest token. Staff usernames never start with it.
pub const SUB_PREFIX: &str = "guest:";

const MAX_TRACKED: usize = 20_000;
/// Scrypt checks running at once (each holds 16 MiB); more answers `Throttled`.
const MAX_HASHING: usize = 8;

#[derive(Default)]
struct Limiter {
    hits: Mutex<HashMap<String, VecDeque<u64>>>,
}

impl Limiter {
    /// Counts one hit for `key` and says whether it is still within `max` per `window`.
    fn allow(&self, key: &str, max: usize, window: u64, now: u64) -> bool {
        let mut m = self.hits.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= MAX_TRACKED && !m.contains_key(key) {
            m.retain(|_, q| q.back().is_some_and(|t| now.saturating_sub(*t) < window));
            if m.len() >= MAX_TRACKED {
                return false;
            }
        }
        let q = m.entry(key.to_string()).or_default();
        while q.front().is_some_and(|t| now.saturating_sub(*t) >= window) {
            q.pop_front();
        }
        if q.len() >= max {
            return false;
        }
        q.push_back(now);
        true
    }
}

pub struct GuestService {
    dir: Arc<dyn GuestDirectory>,
    mailer: Option<Arc<dyn Mailer>>,
    policy: GuestPolicy,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    limiter: Limiter,
    ended: Mutex<HashMap<String, u64>>,
    hashing: AtomicUsize,
    dummy: String,
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_token() -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).ok()?;
    Some(URL_SAFE_NO_PAD.encode(b))
}

/// The email as stored, or why it cannot be used.
pub fn normalize_email(raw: &str) -> Result<String, GuestError> {
    let e = raw.trim().to_lowercase();
    let ok = e.len() <= 254
        && !e.chars().any(|c| c.is_control() || c.is_whitespace())
        && e.split_once('@').is_some_and(|(l, d)| {
            !l.is_empty()
                && l.len() <= 64
                && d.contains('.')
                && !d.starts_with('.')
                && !d.ends_with('.')
                && !d.contains("..")
                && !d.contains('@')
        });
    if ok {
        Ok(e)
    } else {
        Err(GuestError::Invalid(
            "email",
            "Enter a valid email address.".into(),
        ))
    }
}

impl GuestService {
    pub fn new(
        dir: Arc<dyn GuestDirectory>,
        mailer: Option<Arc<dyn Mailer>>,
        policy: GuestPolicy,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<GuestService, GuestDirError> {
        let since = now().saturating_sub(policy.ttl_secs);
        let ended: HashMap<String, u64> = dir.recently_ended(since)?.into_iter().collect();
        let dummy = hash_pin_random("guest-dummy-password").ok_or(GuestDirError::Backend)?;
        Ok(GuestService {
            dir,
            mailer,
            policy,
            now,
            limiter: Limiter::default(),
            ended: Mutex::new(ended),
            hashing: AtomicUsize::new(0),
            dummy,
        })
    }

    /// Replaces the onboarding questions (the server sets them from the manifest when guests are switched on).
    pub(crate) fn set_questions(&mut self, q: Vec<ProfileField>) {
        self.policy.profile = q;
    }

    pub fn ttl_secs(&self) -> u64 {
        self.policy.ttl_secs
    }

    fn now(&self) -> u64 {
        (self.now)()
    }

    /// Why a password cannot be used, or None.
    pub fn password_problem(&self, password: &str, email: &str) -> Option<String> {
        let n = password.chars().count();
        if n < self.policy.min_password {
            return Some(format!(
                "Use at least {} characters.",
                self.policy.min_password
            ));
        }
        if password.len() > self.policy.max_password {
            return Some(format!(
                "Use at most {} characters.",
                self.policy.max_password
            ));
        }
        let first = password.chars().next();
        if password.chars().all(|c| Some(c) == first) {
            return Some("Not one repeated character.".into());
        }
        if password.to_lowercase() == email {
            return Some("The password cannot be your email.".into());
        }
        None
    }

    fn gate(&self) -> Result<HashingPermit<'_>, GuestError> {
        if self.hashing.fetch_add(1, Ordering::SeqCst) >= MAX_HASHING {
            self.hashing.fetch_sub(1, Ordering::SeqCst);
            return Err(GuestError::Throttled);
        }
        Ok(HashingPermit(&self.hashing))
    }

    pub fn signup(
        &self,
        ip: &str,
        name: &str,
        email: &str,
        password: &str,
    ) -> Result<GuestIdentity, GuestError> {
        self.signup_with_profile(ip, name, email, password, &Value::Null)
    }

    /// Checks the profile answers against the declared questions: only declared keys, trimmed text no longer than
    /// the limit, and every required one filled in. `None` means every question is unanswered.
    pub fn validate_profile(&self, raw: &Value) -> Result<Map<String, Value>, GuestError> {
        let given = raw.as_object().cloned().unwrap_or_default();
        let mut out = Map::new();
        for f in &self.policy.profile {
            let text = match given.get(&f.key) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(t)) => t.trim().to_string(),
                Some(_) => {
                    return Err(GuestError::Invalid(
                        leak(&f.key),
                        format!("{}: send text.", f.label),
                    ))
                }
            };
            if text.chars().count() > f.max_length || text.chars().any(|c| c.is_control()) {
                return Err(GuestError::Invalid(
                    leak(&f.key),
                    format!("{}: use at most {} characters.", f.label, f.max_length),
                ));
            }
            if text.is_empty() {
                if f.required {
                    return Err(GuestError::Invalid(
                        leak(&f.key),
                        format!("{} is required.", f.label),
                    ));
                }
            } else {
                out.insert(f.key.clone(), Value::String(text));
            }
        }
        Ok(out)
    }

    pub fn signup_with_profile(
        &self,
        ip: &str,
        name: &str,
        email: &str,
        password: &str,
        profile: &Value,
    ) -> Result<GuestIdentity, GuestError> {
        let now = self.now();
        if !self.limiter.allow(
            &format!("su:{ip}"),
            self.policy.signup_per_ip,
            self.policy.signup_window_secs,
            now,
        ) {
            return Err(GuestError::Throttled);
        }
        let name = name.trim();
        if name.is_empty()
            || name.chars().count() > self.policy.max_name
            || name.chars().any(|c| c.is_control())
        {
            return Err(GuestError::Invalid("name", "Enter your name.".into()));
        }
        let email = normalize_email(email)?;
        if let Some(why) = self.password_problem(password, &email) {
            return Err(GuestError::Invalid("password", why));
        }
        let profile = self.validate_profile(profile)?;
        let _permit = self.gate()?;
        let hash = hash_pin_random(password).ok_or(GuestError::Unavailable)?;
        match self.dir.create(&email, name, &hash, &profile, now) {
            Ok(rec) => Ok(identity_of(&rec)),
            Err(GuestDirError::Exists) => Err(GuestError::Exists),
            Err(_) => Err(GuestError::Unavailable),
        }
    }

    pub fn login(
        &self,
        ip: &str,
        email: &str,
        password: &str,
    ) -> Result<GuestIdentity, GuestError> {
        let now = self.now();
        let email = match normalize_email(email) {
            Ok(e) => e,
            Err(_) => return Err(GuestError::Denied),
        };
        if !self.limiter.allow(
            &format!("lip:{ip}"),
            self.policy.login_per_ip,
            self.policy.window_secs,
            now,
        ) || !self.limiter.allow(
            &format!("le:{email}"),
            self.policy.login_per_email,
            self.policy.window_secs,
            now,
        ) {
            return Err(GuestError::Throttled);
        }
        if password.is_empty() || password.len() > self.policy.max_password {
            return Err(GuestError::Denied);
        }
        let _permit = self.gate()?;
        let found = self
            .dir
            .by_email(&email)
            .map_err(|_| GuestError::Unavailable)?;
        // An unknown email is checked against a dummy hash so it costs the same.
        let hash = found
            .as_ref()
            .map(|r| r.hash.as_str())
            .unwrap_or(&self.dummy);
        let matched = check_secret(password, hash);
        match found {
            Some(rec) if matched && rec.active => Ok(identity_of(&rec)),
            _ => Err(GuestError::Denied),
        }
    }

    /// Who this token's guest is, for the profile page and for pre-filling their forms.
    pub fn identity(&self, sub: &str) -> Result<GuestIdentity, GuestError> {
        let id = sub.strip_prefix(SUB_PREFIX).ok_or(GuestError::Denied)?;
        match self.dir.by_id(id) {
            Ok(Some(rec)) if rec.active => Ok(identity_of(&rec)),
            Ok(_) => Err(GuestError::Denied),
            Err(_) => Err(GuestError::Unavailable),
        }
    }

    /// Changes the guest's name and profile answers (every declared question is re-checked).
    pub fn update_profile(
        &self,
        sub: &str,
        name: Option<&str>,
        profile: &Value,
    ) -> Result<GuestIdentity, GuestError> {
        let current = self.identity(sub)?;
        let name = name.map(str::trim).unwrap_or(&current.name);
        if name.is_empty()
            || name.chars().count() > self.policy.max_name
            || name.chars().any(|c| c.is_control())
        {
            return Err(GuestError::Invalid("name", "Enter your name.".into()));
        }
        let profile = self.validate_profile(profile)?;
        let id = sub.strip_prefix(SUB_PREFIX).ok_or(GuestError::Denied)?;
        self.dir
            .set_profile(id, name, &profile)
            .map_err(|_| GuestError::Unavailable)?;
        self.identity(sub)
    }

    /// Counts one write by this guest and says whether it is within `max` per hour.
    pub fn allow_write(&self, sub: &str, max: usize) -> bool {
        self.limiter
            .allow(&format!("w:{sub}"), max, 3600, self.now())
    }

    /// Ends this guest's sessions (sign-out). `sub` is the token subject.
    pub fn logout(&self, sub: &str) -> Result<(), GuestError> {
        let Some(id) = sub.strip_prefix(SUB_PREFIX) else {
            return Err(GuestError::Denied);
        };
        let now = self.now();
        self.dir
            .end_sessions(id, now)
            .map_err(|_| GuestError::Unavailable)?;
        self.ended
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), now);
        Ok(())
    }

    /// Asks for a reset email. Always answers Ok for a well-formed request, so the answer never
    /// says whether an account exists; only the rate limit can refuse.
    pub fn request_reset(&self, ip: &str, email: &str) -> Result<(), GuestError> {
        let now = self.now();
        let email = normalize_email(email)?;
        if !self.limiter.allow(
            &format!("rip:{ip}"),
            self.policy.reset_per_ip,
            self.policy.window_secs,
            now,
        ) || !self.limiter.allow(
            &format!("re:{email}"),
            self.policy.reset_per_email,
            self.policy.window_secs,
            now,
        ) {
            return Err(GuestError::Throttled);
        }
        let Some(mailer) = self.mailer.clone() else {
            return Err(GuestError::Unavailable);
        };
        let Ok(Some(rec)) = self.dir.by_email(&email) else {
            return Ok(());
        };
        if !rec.active {
            return Ok(());
        }
        let Some(token) = random_token() else {
            return Err(GuestError::Unavailable);
        };
        if self
            .dir
            .put_reset(
                &rec.id,
                &sha256_hex(&token),
                now + self.policy.reset_ttl_secs,
            )
            .is_err()
        {
            return Err(GuestError::Unavailable);
        }
        let sep = if self.policy.reset_link_base.contains('?') {
            '&'
        } else {
            '?'
        };
        let link = format!("{}{sep}token={token}", self.policy.reset_link_base);
        // A failed send is not told to the caller: that would reveal that the account exists.
        let _ = mailer.send_reset(&rec.email, &rec.name, &link);
        Ok(())
    }

    /// Sets a new password with a reset token (single use) and ends every earlier session.
    pub fn confirm_reset(&self, token: &str, password: &str) -> Result<(), GuestError> {
        let now = self.now();
        if token.is_empty() || token.len() > 200 {
            return Err(GuestError::Denied);
        }
        let Some(id) = self
            .dir
            .take_reset(&sha256_hex(token), now)
            .map_err(|_| GuestError::Unavailable)?
        else {
            return Err(GuestError::Denied);
        };
        let rec = self
            .dir
            .by_id(&id)
            .map_err(|_| GuestError::Unavailable)?
            .ok_or(GuestError::Denied)?;
        if let Some(why) = self.password_problem(password, &rec.email) {
            return Err(GuestError::Invalid("password", why));
        }
        let _permit = self.gate()?;
        let hash = hash_pin_random(password).ok_or(GuestError::Unavailable)?;
        self.dir
            .set_password(&id, &hash, now)
            .map_err(|_| GuestError::Unavailable)?;
        self.ended
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, now);
        Ok(())
    }

    /// Tokens for this guest issued at or before the returned time are refused. Never blocks.
    pub fn not_before(&self, sub: &str) -> Option<u64> {
        let id = sub.strip_prefix(SUB_PREFIX)?;
        self.ended
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .copied()
    }
}

/// The onboarding questions a manifest declares in `guest.profile`, in order.
pub fn questions_from_manifest(m: &ce_manifest::Manifest) -> Vec<ProfileField> {
    m.value["guest"]["profile"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let key = q["key"].as_str()?.to_string();
            Some(ProfileField {
                label: q["label"].as_str().unwrap_or(&key).to_string(),
                required: q["required"].as_bool().unwrap_or(false),
                max_length: q["max_length"].as_u64().unwrap_or(120).clamp(1, 200) as usize,
                key,
            })
        })
        .collect()
}

fn identity_of(rec: &GuestRecord) -> GuestIdentity {
    GuestIdentity {
        sub: format!("{SUB_PREFIX}{}", rec.id),
        name: rec.name.clone(),
        email: rec.email.clone(),
        profile: rec.profile.clone(),
    }
}

/// An error path names a profile key; the keys are manifest constants, so keeping one for the life of
/// the process is bounded by the manifest.
fn leak(key: &str) -> &'static str {
    static KEYS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut k = KEYS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(found) = k.iter().find(|x| **x == key) {
        return found;
    }
    let b: &'static str = Box::leak(key.to_string().into_boxed_str());
    k.push(b);
    b
}

struct HashingPermit<'a>(&'a AtomicUsize);
impl Drop for HashingPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An in-memory directory for tests and for hosts that keep guests elsewhere in development.
#[derive(Default)]
pub struct MemoryGuests {
    inner: Mutex<MemInner>,
}

#[derive(Default)]
struct MemInner {
    by_id: HashMap<String, GuestRecord>,
    ended: HashMap<String, u64>,
    resets: HashMap<String, (String, u64)>,
    n: u64,
}

impl GuestDirectory for MemoryGuests {
    fn create(
        &self,
        email: &str,
        name: &str,
        hash: &str,
        profile: &Map<String, Value>,
        _now: u64,
    ) -> Result<GuestRecord, GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if m.by_id.values().any(|r| r.email == email) {
            return Err(GuestDirError::Exists);
        }
        m.n += 1;
        let rec = GuestRecord {
            id: format!("00000000-0000-4000-8000-{:012x}", m.n),
            email: email.to_string(),
            name: name.to_string(),
            hash: hash.to_string(),
            active: true,
            profile: profile.clone(),
        };
        m.by_id.insert(rec.id.clone(), rec.clone());
        Ok(rec)
    }
    fn set_profile(
        &self,
        id: &str,
        name: &str,
        profile: &Map<String, Value>,
    ) -> Result<(), GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let r = m.by_id.get_mut(id).ok_or(GuestDirError::Missing)?;
        r.name = name.to_string();
        r.profile = profile.clone();
        Ok(())
    }
    fn by_email(&self, email: &str) -> Result<Option<GuestRecord>, GuestDirError> {
        let m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.by_id.values().find(|r| r.email == email).cloned())
    }
    fn by_id(&self, id: &str) -> Result<Option<GuestRecord>, GuestDirError> {
        let m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.by_id.get(id).cloned())
    }
    fn set_password(&self, id: &str, hash: &str, now: u64) -> Result<(), GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let r = m.by_id.get_mut(id).ok_or(GuestDirError::Missing)?;
        r.hash = hash.to_string();
        m.ended.insert(id.to_string(), now);
        Ok(())
    }
    fn end_sessions(&self, id: &str, now: u64) -> Result<(), GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !m.by_id.contains_key(id) {
            return Err(GuestDirError::Missing);
        }
        m.ended.insert(id.to_string(), now);
        Ok(())
    }
    fn recently_ended(&self, since: u64) -> Result<Vec<(String, u64)>, GuestDirError> {
        let m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.ended
            .iter()
            .filter(|(_, t)| **t >= since)
            .map(|(k, t)| (k.clone(), *t))
            .collect())
    }
    fn put_reset(&self, id: &str, token_hash: &str, expires: u64) -> Result<(), GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        m.resets
            .insert(token_hash.to_string(), (id.to_string(), expires));
        Ok(())
    }
    fn take_reset(&self, token_hash: &str, now: u64) -> Result<Option<String>, GuestDirError> {
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(match m.resets.remove(token_hash) {
            Some((id, exp)) if exp > now => Some(id),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicU64;

    struct Sent(Mutex<Vec<(String, String)>>);
    impl Mailer for Sent {
        fn send_reset(&self, to: &str, _name: &str, link: &str) -> Result<(), MailError> {
            self.0
                .lock()
                .unwrap()
                .push((to.to_string(), link.to_string()));
            Ok(())
        }
    }

    fn svc() -> (GuestService, Arc<Sent>, Arc<AtomicU64>) {
        let clock = Arc::new(AtomicU64::new(1_000_000));
        let c = clock.clone();
        let sent = Arc::new(Sent(Mutex::new(vec![])));
        let policy = GuestPolicy {
            reset_link_base: "https://x.test/reset".into(),
            ..GuestPolicy::default()
        };
        let s = GuestService::new(
            Arc::new(MemoryGuests::default()),
            Some(sent.clone()),
            policy,
            Arc::new(move || c.load(Ordering::SeqCst)),
        )
        .unwrap();
        (s, sent, clock)
    }

    #[test]
    fn signup_then_login_and_duplicates() {
        let (s, _, _) = svc();
        let me = s
            .signup("1.1.1.1", "Jordan", " Jordan@Example.COM ", "correct horse")
            .unwrap();
        assert!(me.sub.starts_with(SUB_PREFIX));
        assert_eq!(
            s.signup("1.1.1.1", "J", "jordan@example.com", "another one 1"),
            Err(GuestError::Exists)
        );
        let back = s
            .login("1.1.1.1", "jordan@example.com", "correct horse")
            .unwrap();
        assert_eq!(back.sub, me.sub);
        assert_eq!(back.name, "Jordan");
    }

    #[test]
    fn wrong_unknown_and_malformed_are_the_same_denial() {
        let (s, _, _) = svc();
        s.signup("1.1.1.1", "Jordan", "j@example.com", "correct horse")
            .unwrap();
        assert_eq!(
            s.login("2.2.2.2", "j@example.com", "wrong password"),
            Err(GuestError::Denied)
        );
        assert_eq!(
            s.login("2.2.2.2", "nobody@example.com", "correct horse"),
            Err(GuestError::Denied)
        );
        assert_eq!(
            s.login("2.2.2.2", "not-an-email", "correct horse"),
            Err(GuestError::Denied)
        );
        assert_eq!(
            s.login("2.2.2.2", "j@example.com", ""),
            Err(GuestError::Denied)
        );
    }

    #[test]
    fn weak_passwords_and_bad_fields_say_why() {
        let (s, _, _) = svc();
        for (pw, field) in [
            ("short", "password"),
            ("aaaaaaaaaa", "password"),
            ("j@example.com", "password"),
        ] {
            match s.signup("1.1.1.1", "Jordan", "j@example.com", pw) {
                Err(GuestError::Invalid(f, msg)) => {
                    assert_eq!(f, field);
                    assert!(!msg.is_empty());
                }
                other => panic!("{pw}: {other:?}"),
            }
        }
        assert!(matches!(
            s.signup("1.1.1.1", "  ", "j@example.com", "correct horse"),
            Err(GuestError::Invalid("name", _))
        ));
        assert!(matches!(
            s.signup("1.1.1.1", "J", "nope", "correct horse"),
            Err(GuestError::Invalid("email", _))
        ));
        assert!(matches!(
            s.signup("1.1.1.1", "J", "a@b", "correct horse"),
            Err(GuestError::Invalid("email", _))
        ));
    }

    #[test]
    fn login_is_rate_limited_per_email_and_resets_after_the_window() {
        let (s, _, clock) = svc();
        s.signup("1.1.1.1", "Jordan", "j@example.com", "correct horse")
            .unwrap();
        for i in 0..8 {
            assert_eq!(
                s.login(&format!("9.9.9.{i}"), "j@example.com", "wrong password"),
                Err(GuestError::Denied)
            );
        }
        // The ninth try is refused even with the right password.
        assert_eq!(
            s.login("9.9.9.99", "j@example.com", "correct horse"),
            Err(GuestError::Throttled)
        );
        clock.fetch_add(15 * 60 + 1, Ordering::SeqCst);
        assert!(s
            .login("9.9.9.99", "j@example.com", "correct horse")
            .is_ok());
    }

    #[test]
    fn signup_is_rate_limited_per_ip() {
        let (s, _, _) = svc();
        for i in 0..10 {
            s.signup(
                "3.3.3.3",
                "G",
                &format!("g{i}@example.com"),
                "correct horse",
            )
            .unwrap();
        }
        assert_eq!(
            s.signup("3.3.3.3", "G", "g99@example.com", "correct horse"),
            Err(GuestError::Throttled)
        );
        assert!(s
            .signup("4.4.4.4", "G", "g99@example.com", "correct horse")
            .is_ok());
    }

    #[test]
    fn logout_sets_a_cutoff_for_that_guest_only() {
        let (s, _, clock) = svc();
        let a = s
            .signup("1.1.1.1", "A", "a@example.com", "correct horse")
            .unwrap();
        let b = s
            .signup("1.1.1.1", "B", "b@example.com", "correct horse")
            .unwrap();
        assert_eq!(s.not_before(&a.sub), None);
        clock.fetch_add(5, Ordering::SeqCst);
        s.logout(&a.sub).unwrap();
        assert_eq!(s.not_before(&a.sub), Some(1_000_005));
        assert_eq!(s.not_before(&b.sub), None);
        assert_eq!(s.not_before("some-staff-user"), None);
        assert!(s.logout("staff-user").is_err());
    }

    #[test]
    fn reset_is_silent_single_use_expiring_and_ends_sessions() {
        let (s, sent, clock) = svc();
        let me = s
            .signup("1.1.1.1", "Jordan", "j@example.com", "correct horse")
            .unwrap();
        // An unknown email gets the same answer and no mail.
        assert!(s.request_reset("5.5.5.5", "nobody@example.com").is_ok());
        assert!(sent.0.lock().unwrap().is_empty());
        assert!(s.request_reset("5.5.5.5", "j@example.com").is_ok());
        let link = sent.0.lock().unwrap()[0].1.clone();
        assert!(link.starts_with("https://x.test/reset?token="));
        let token = link.split("token=").nth(1).unwrap().to_string();
        clock.fetch_add(10, Ordering::SeqCst);
        assert_eq!(s.confirm_reset(&token, "brand new pass"), Ok(()));
        assert_eq!(
            s.confirm_reset(&token, "brand new pass"),
            Err(GuestError::Denied)
        ); // single use
        assert_eq!(s.not_before(&me.sub), Some(1_000_010));
        assert_eq!(
            s.login("1.1.1.1", "j@example.com", "correct horse"),
            Err(GuestError::Denied)
        );
        assert!(s
            .login("1.1.1.1", "j@example.com", "brand new pass")
            .is_ok());
        // An expired token is refused.
        clock.fetch_add(60, Ordering::SeqCst);
        s.request_reset("6.6.6.6", "j@example.com").unwrap();
        let t2 = sent.0.lock().unwrap()[1]
            .1
            .split("token=")
            .nth(1)
            .unwrap()
            .to_string();
        clock.fetch_add(3601, Ordering::SeqCst);
        assert_eq!(
            s.confirm_reset(&t2, "another new one"),
            Err(GuestError::Denied)
        );
    }

    #[test]
    fn reset_with_a_weak_password_is_refused_with_a_reason() {
        let (s, sent, _) = svc();
        s.signup("1.1.1.1", "Jordan", "j@example.com", "correct horse")
            .unwrap();
        s.request_reset("5.5.5.5", "j@example.com").unwrap();
        let token = sent.0.lock().unwrap()[0]
            .1
            .split("token=")
            .nth(1)
            .unwrap()
            .to_string();
        assert!(matches!(
            s.confirm_reset(&token, "short"),
            Err(GuestError::Invalid("password", _))
        ));
    }

    #[test]
    fn reset_requests_are_limited_per_email() {
        let (s, _, _) = svc();
        s.signup("1.1.1.1", "Jordan", "j@example.com", "correct horse")
            .unwrap();
        for i in 0..3 {
            assert!(s
                .request_reset(&format!("7.7.7.{i}"), "j@example.com")
                .is_ok());
        }
        assert_eq!(
            s.request_reset("7.7.7.9", "j@example.com"),
            Err(GuestError::Throttled)
        );
    }

    #[test]
    fn a_restart_remembers_recent_sign_outs() {
        let dir: Arc<dyn GuestDirectory> = Arc::new(MemoryGuests::default());
        let mk = |d: Arc<dyn GuestDirectory>| {
            GuestService::new(d, None, GuestPolicy::default(), Arc::new(|| 2_000_000)).unwrap()
        };
        let first = mk(dir.clone());
        let me = first
            .signup("1.1.1.1", "J", "j@example.com", "correct horse")
            .unwrap();
        first.logout(&me.sub).unwrap();
        let second = mk(dir);
        assert_eq!(second.not_before(&me.sub), Some(2_000_000));
    }

    fn svc_with_questions() -> GuestService {
        let policy = GuestPolicy {
            profile: vec![
                ProfileField {
                    key: "dept".into(),
                    label: "Department".into(),
                    required: true,
                    max_length: 20,
                },
                ProfileField {
                    key: "ext".into(),
                    label: "Extension".into(),
                    required: false,
                    max_length: 8,
                },
            ],
            ..GuestPolicy::default()
        };
        GuestService::new(
            Arc::new(MemoryGuests::default()),
            None,
            policy,
            Arc::new(|| 1_000_000),
        )
        .unwrap()
    }

    #[test]
    fn onboarding_questions_are_required_trimmed_bounded_and_stored() {
        let s = svc_with_questions();
        let ask = |p: Value, email: &str| {
            s.signup_with_profile("1.1.1.1", "Tanya", email, "correct horse", &p)
        };
        assert!(matches!(
            ask(json!({}), "a@example.com"),
            Err(GuestError::Invalid("dept", _))
        ));
        assert!(matches!(
            ask(json!({"dept": "   "}), "a@example.com"),
            Err(GuestError::Invalid("dept", _))
        ));
        assert!(matches!(
            ask(json!({"dept": "x".repeat(21)}), "a@example.com"),
            Err(GuestError::Invalid("dept", _))
        ));
        assert!(matches!(
            ask(json!({"dept": 5}), "a@example.com"),
            Err(GuestError::Invalid("dept", _))
        ));
        let me = ask(
            json!({"dept": "  Admissions ", "ext": "2210", "role": "boss", "nonsense": 1}),
            "a@example.com",
        )
        .unwrap();
        // Only declared questions are kept, trimmed.
        assert_eq!(
            Value::Object(me.profile.clone()),
            json!({"dept": "Admissions", "ext": "2210"})
        );
        let back = s
            .login("1.1.1.1", "a@example.com", "correct horse")
            .unwrap();
        assert_eq!(back.profile, me.profile);
        assert_eq!(back.email, "a@example.com");
        // Optional questions may stay empty.
        assert_eq!(
            Value::Object(
                ask(json!({"dept": "Academics"}), "b@example.com")
                    .unwrap()
                    .profile
            ),
            json!({"dept": "Academics"})
        );
    }

    #[test]
    fn a_guest_can_change_their_own_answers_and_name() {
        let s = svc_with_questions();
        let me = s
            .signup_with_profile(
                "1.1.1.1",
                "Tanya",
                "a@example.com",
                "correct horse",
                &json!({"dept": "Admissions"}),
            )
            .unwrap();
        let after = s
            .update_profile(
                &me.sub,
                Some(" Tanya B "),
                &json!({"dept": "Academics", "ext": "99"}),
            )
            .unwrap();
        assert_eq!(
            (after.name.as_str(), Value::Object(after.profile.clone())),
            ("Tanya B", json!({"dept": "Academics", "ext": "99"}))
        );
        assert_eq!(s.identity(&me.sub).unwrap().profile, after.profile);
        assert!(matches!(
            s.update_profile(&me.sub, None, &json!({"ext": "1"})),
            Err(GuestError::Invalid("dept", _))
        ));
        assert!(matches!(
            s.update_profile(&me.sub, Some(" "), &json!({"dept": "X"})),
            Err(GuestError::Invalid("name", _))
        ));
        assert!(s.identity("staff-user").is_err());
    }
}
