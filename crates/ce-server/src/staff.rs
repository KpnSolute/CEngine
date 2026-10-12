//! Staff records and their administration: who may sign in with a PIN, with which roles, and the
//! calls that create staff, change roles, switch an account off and set or change a PIN.
//!
//! [`StaffDirectory`] is where records live: a list read from a file at start ([`MemoryDirectory`],
//! read-only in that mode) or a table in the installation's database (the host supplies it).
//! [`StaffAdmin`] is what the HTTP routes call. Hashes never leave this module's callers: nothing
//! here returns one to a client, and no PIN is stored or logged.

/// One staff account. `username` is the lower-cased key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffRecord {
    pub username: String,
    pub hash: String,
    pub roles: Vec<String>,
    pub active: bool,
    /// The next sign-in may only set a new PIN.
    pub must_rotate: bool,
    /// Seconds since the epoch of the last PIN set or change (0 when unknown).
    pub pin_updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryFailure {
    Missing,
    Exists,
    /// The directory was loaded from a file and cannot be changed from the API.
    ReadOnly,
    Backend,
}

/// Where staff records live. Called off the async runtime, so implementations may block.
/// Every change names the actor (a username) so the host can keep a trail; no PIN or hash goes in it.
pub trait StaffDirectory: Send + Sync {
    fn get(&self, username: &str) -> Result<Option<StaffRecord>, DirectoryFailure>;
    fn list(&self) -> Result<Vec<StaffRecord>, DirectoryFailure>;
    fn create(&self, record: StaffRecord, actor: &str) -> Result<(), DirectoryFailure>;
    /// Sets a new hash, clears or sets `must_rotate`, and stamps the time.
    fn set_pin(
        &self,
        username: &str,
        hash: &str,
        must_rotate: bool,
        now: u64,
        actor: &str,
    ) -> Result<(), DirectoryFailure>;
    fn set_roles(
        &self,
        username: &str,
        roles: &[String],
        actor: &str,
    ) -> Result<(), DirectoryFailure>;
    fn set_active(&self, username: &str, active: bool, actor: &str)
        -> Result<(), DirectoryFailure>;
    fn writable(&self) -> bool;
}

/// Staff records in memory. Read from the staff file at start; changes last until restart, so the
/// file mode is [`MemoryDirectory::read_only`].
pub struct MemoryDirectory {
    records: std::sync::Mutex<std::collections::HashMap<String, StaffRecord>>,
    writable: bool,
}

impl MemoryDirectory {
    pub fn new(records: Vec<StaffRecord>, writable: bool) -> Self {
        MemoryDirectory {
            records: std::sync::Mutex::new(
                records
                    .into_iter()
                    .map(|r| (r.username.clone(), r))
                    .collect(),
            ),
            writable,
        }
    }

    pub fn read_only(records: Vec<StaffRecord>) -> Self {
        Self::new(records, false)
    }

    fn map(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, StaffRecord>> {
        self.records.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn edit(
        &self,
        username: &str,
        f: impl FnOnce(&mut StaffRecord),
    ) -> Result<(), DirectoryFailure> {
        if !self.writable {
            return Err(DirectoryFailure::ReadOnly);
        }
        self.map()
            .get_mut(username)
            .map(f)
            .ok_or(DirectoryFailure::Missing)
    }
}

impl StaffDirectory for MemoryDirectory {
    fn get(&self, username: &str) -> Result<Option<StaffRecord>, DirectoryFailure> {
        Ok(self.map().get(username).cloned())
    }

    fn list(&self) -> Result<Vec<StaffRecord>, DirectoryFailure> {
        let mut all: Vec<StaffRecord> = self.map().values().cloned().collect();
        all.sort_by(|a, b| a.username.cmp(&b.username));
        Ok(all)
    }

    fn create(&self, record: StaffRecord, _actor: &str) -> Result<(), DirectoryFailure> {
        if !self.writable {
            return Err(DirectoryFailure::ReadOnly);
        }
        let mut m = self.map();
        if m.contains_key(&record.username) {
            return Err(DirectoryFailure::Exists);
        }
        m.insert(record.username.clone(), record);
        Ok(())
    }

    fn set_pin(
        &self,
        username: &str,
        hash: &str,
        must_rotate: bool,
        now: u64,
        _actor: &str,
    ) -> Result<(), DirectoryFailure> {
        self.edit(username, |r| {
            r.hash = hash.to_string();
            r.must_rotate = must_rotate;
            r.pin_updated_at = now;
        })
    }

    fn set_roles(
        &self,
        username: &str,
        roles: &[String],
        _actor: &str,
    ) -> Result<(), DirectoryFailure> {
        self.edit(username, |r| r.roles = roles.to_vec())
    }

    fn set_active(
        &self,
        username: &str,
        active: bool,
        _actor: &str,
    ) -> Result<(), DirectoryFailure> {
        self.edit(username, |r| r.active = active)
    }

    fn writable(&self) -> bool {
        self.writable
    }
}

/// What a client may see of an account. Never a hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaffInfo {
    pub username: String,
    pub roles: Vec<String>,
    pub active: bool,
    pub must_rotate: bool,
    pub pin_updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminError {
    /// The request breaks a rule; the text is safe to show.
    Invalid(String),
    /// The caller may not do this (a role above their own, or their own account).
    Forbidden(String),
    /// The current PIN was wrong.
    WrongPin,
    Throttled,
    Missing,
    Exists,
    ReadOnly,
    Unavailable,
}

/// The staff calls behind `/_auth/pin` and `/_staff`. `actor_roles` are the caller's roles from
/// their token; a caller can grant and change only roles at or below their own.
pub trait StaffAdmin: Send + Sync {
    fn change_pin(&self, username: &str, current: &str, new: &str) -> Result<(), AdminError>;
    fn list(&self) -> Result<Vec<StaffInfo>, AdminError>;
    fn create(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        roles: &[String],
        pin: &str,
    ) -> Result<StaffInfo, AdminError>;
    fn update(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        roles: Option<&[String]>,
        active: Option<bool>,
    ) -> Result<StaffInfo, AdminError>;
    /// Sets a temporary PIN; the person must pick their own at the next sign-in.
    fn reset_pin(
        &self,
        actor: &str,
        actor_roles: &[String],
        username: &str,
        pin: &str,
    ) -> Result<StaffInfo, AdminError>;
}
