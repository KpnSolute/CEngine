//! CEngine store contract (draft v1) plus an in-memory reference implementation.
//!
//! Every operation takes a `&TenantScope`, so a call without a verified tenant does not
//! compile. Rules any backend must keep (the Postgres backend will run the same tests):
//! 1. Tenants are isolated. A record of another tenant is `NotFound`, never "forbidden",
//!    so ids do not leak across tenants.
//! 2. Updates and deletes carry the version the caller last saw; a mismatch is
//!    `VersionConflict` and changes nothing.
//! 3. Delete is soft: the record disappears from reads and keeps its version history.
//! 4. An idempotency key replays the first result for the same payload and is refused
//!    for a different payload. Keys are scoped per tenant.
//!
//! The trait is synchronous for now; the Postgres backend will move it to async before 0.1.

mod memory;

pub use ce_auth::TenantScope;
pub use memory::MemoryStore;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub id: String,
    pub entity: String,
    /// Starts at 1 and increases by 1 on every successful update or delete.
    pub version: u64,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreError {
    NotFound,
    VersionConflict { current: u64 },
    IdempotencyKeyReused,
    BadData(&'static str),
}

pub trait Store {
    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError>;

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError>;

    /// Replaces `data` when `expected_version` matches the stored version.
    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError>;

    /// Soft-deletes when `expected_version` matches.
    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError>;
}
