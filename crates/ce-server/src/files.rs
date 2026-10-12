//! Files: `file` fields hold metadata (`key`, `sha256`, `size`, `content_type`, `name`) and the
//! bytes live in a [`BlobStore`] the host provides. CEngine never talks to a storage provider:
//! Infajn's storage connector implements the trait. The server keeps keys under the tenant, so a
//! store cannot mix tenants even if it wants to, and addresses bytes by content hash.
//!
//! `PUT /v1/{inst}/_files/{entity}/{id}/{field}` stores the body and sets the field (needs write
//! access, `If-Match`, and respects freezes and audit like any patch). `GET` on the same path
//! returns the bytes (needs read access). Replacing or clearing a file leaves the old bytes in
//! the store; removing them is the connector's retention job.

use ce_auth::TenantScope;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;

/// Where file bytes live. Implementations are provided by the host (Infajn's storage connector in
/// production, [`MemoryBlobs`] locally and in tests). `key` is already prefixed by the tenant.
pub trait BlobStore: Send + Sync {
    fn put(
        &self,
        scope: &TenantScope,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), String>;
    /// The bytes and their content type, or `None` when there is no such object.
    fn get(&self, scope: &TenantScope, key: &str) -> Result<Option<(Vec<u8>, String)>, String>;
}

/// Keeps blobs in memory. For tests and local runs.
#[derive(Default)]
pub struct MemoryBlobs {
    items: Mutex<HashMap<String, (Vec<u8>, String)>>,
}

impl MemoryBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlobStore for MemoryBlobs {
    fn put(
        &self,
        scope: &TenantScope,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), String> {
        // Keyed by tenant as well, as a real store must be.
        let full = format!("{}/{key}", scope.tenant_id());
        self.items
            .lock()
            .map_err(|_| "lock".to_string())?
            .insert(full, (bytes.to_vec(), content_type.to_string()));
        Ok(())
    }

    fn get(&self, scope: &TenantScope, key: &str) -> Result<Option<(Vec<u8>, String)>, String> {
        let full = format!("{}/{key}", scope.tenant_id());
        Ok(self
            .items
            .lock()
            .map_err(|_| "lock".to_string())?
            .get(&full)
            .cloned())
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The object key: tenant first, then where the file belongs, then its content hash.
pub(crate) fn object_key(
    scope: &TenantScope,
    entity: &str,
    id: &str,
    field: &str,
    sha: &str,
) -> String {
    format!("{}/{entity}/{id}/{field}/{sha}", scope.tenant_id())
}

/// A file name that is safe inside a header value and a path.
pub(crate) fn clean_name(raw: &str) -> String {
    let n: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .take(120)
        .collect();
    let n = n.trim().trim_start_matches('.').to_string();
    if n.is_empty() {
        "file".into()
    } else {
        n
    }
}
