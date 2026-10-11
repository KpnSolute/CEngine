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
//! 5. `atomic` commits all of a closure's writes or none of them.
//!
//! The trait is synchronous for now; the Postgres backend will move it to async before 0.1.

#[cfg(feature = "contract-tests")]
pub mod contract;
mod memory;

pub use ce_auth::TenantScope;
pub use ce_query::{ListRequest, QueryError, SortDir, SortKey};
pub use memory::MemoryStore;
use serde_json::{Map, Value};

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
    VersionConflict {
        current: u64,
    },
    IdempotencyKeyReused,
    BadData(&'static str),
    /// The write broke a rule the database enforces (unique, reference, check, not null).
    /// The text names the kind, never the table or value, so it is safe to show callers.
    Constraint(&'static str),
    /// The list request broke a query rule (unknown field, unindexed sort, bad filter...).
    Query(QueryError),
    /// This store cannot do what was asked (for example, listing).
    Unsupported,
    /// The backend failed (connection, SQL). The message is for logs, not for callers.
    Backend(String),
    /// A transaction was rolled back because its closure asked to stop; the caller already
    /// holds the reason. Never shown to users.
    Aborted,
}

/// One page of a list. `next` is the cursor for the following page (the last row's sort-key
/// values, then its id), or `None` on the last page.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub records: Vec<Record>,
    pub next: Option<Vec<Value>>,
}

pub trait Store {
    /// Lists one tenant's records of `entity`. Stores that cannot query return `Unsupported`.
    fn list(
        &self,
        _scope: &TenantScope,
        _entity: &str,
        _req: &ListRequest,
    ) -> Result<Page, StoreError> {
        Err(StoreError::Unsupported)
    }

    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError>;

    /// Like `insert`, for a body whose `derived` fields were computed by the host from the
    /// store's current contents (so they can differ between two identical requests). The
    /// idempotency check compares only `caller`, the body as the client sent it; the record
    /// stored is `caller` with `derived` merged over it. A store that cannot compare that way
    /// returns `Unsupported` when there are derived fields, rather than reporting a retry as a
    /// reused key; with none it is `insert`.
    fn insert_derived(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: Value,
        derived: Map<String, Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        if !derived.is_empty() {
            return Err(StoreError::Unsupported);
        }
        self.insert(scope, entity, caller, idempotency_key)
    }

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

    /// Runs `f` as one transaction: every write made through the store handed to `f` commits
    /// together when `f` returns `Ok`, and none is kept when it returns `Err`. Reads inside `f`
    /// see its own writes. Nested calls join the running transaction. Stores that cannot do this
    /// return `Unsupported` without calling `f`.
    fn atomic(
        &self,
        _f: &mut dyn FnMut(&dyn Store) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unsupported)
    }

    /// Like `get`, and inside `atomic` also locks the record until the transaction ends, so two
    /// transactions that recompute the same parent take turns. Outside a transaction it is `get`.
    fn get_locked(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
    ) -> Result<Record, StoreError> {
        self.get(scope, entity, id)
    }

    /// `attr` of every live record of `entity` whose `ref_field` equals `parent_id`, in any
    /// order; a missing value is `null`. With no `attr`, one `true` per record. Used for sums
    /// and counts over child records, so it reads all of them, not a page.
    fn child_values(
        &self,
        _scope: &TenantScope,
        _entity: &str,
        _ref_field: &str,
        _parent_id: &str,
        _attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        Err(StoreError::Unsupported)
    }
}
