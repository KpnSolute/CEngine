//! Copying legacy files into the blob store with proof. Each file goes through the same steps
//! in order: read the source, hash it, check the hash and size the legacy side recorded, copy it
//! under the key the file endpoints expect, read it back and hash that, then link the metadata
//! to the record. A file that fails a step is reported with a code and nothing else: no name,
//! no bytes, no path. Nothing here knows about a database or a provider.

use crate::files::{object_key, sha256_hex};
use crate::BlobStore;
use ce_auth::TenantScope;
use ce_store::{Store, StoreError};
use serde_json::{json, Value};
use std::collections::HashSet;

/// Where the legacy bytes are read from.
pub trait SourceFiles {
    /// The bytes, or `None` when the source has no such file.
    fn read(&self, source_ref: &str) -> Result<Option<Vec<u8>>, String>;
}

/// One file to move and the record field it belongs to.
pub struct FileItem {
    pub source_id: String,
    pub source_ref: String,
    pub entity: String,
    pub record_id: String,
    pub field: String,
    pub name: String,
    pub content_type: String,
    /// What the legacy side recorded, when it recorded anything.
    pub expected_sha256: Option<String>,
    pub expected_size: Option<u64>,
}

#[derive(Debug, Default, PartialEq)]
pub struct FileReport {
    /// Source ids copied, read back, hash-checked and linked.
    pub verified: Vec<String>,
    /// Source id and code, in the order tried.
    pub rejected: Vec<(String, &'static str)>,
    /// Files whose bytes were already seen earlier in this batch (kept, counted).
    pub duplicates: usize,
    /// Source refs offered as the whole source set that no item points at.
    pub orphan_sources: Vec<String>,
}

fn reject(r: &mut FileReport, id: &str, code: &'static str) {
    r.rejected.push((id.to_string(), code));
}

/// Moves `items` into `blobs` and links each to its record in `store`. `all_sources` is every
/// ref the source holds; the ones no item uses are reported as orphans. Running it again over
/// the same items writes the same keys and finds the links already in place.
pub fn migrate_files<S: Store>(
    store: &S,
    blobs: &dyn BlobStore,
    source: &dyn SourceFiles,
    scope: &TenantScope,
    items: &[FileItem],
    all_sources: &[String],
) -> FileReport {
    let mut rep = FileReport::default();
    let mut seen_hashes: HashSet<String> = HashSet::new();
    let used: HashSet<&str> = items.iter().map(|i| i.source_ref.as_str()).collect();
    rep.orphan_sources = all_sources
        .iter()
        .filter(|s| !used.contains(s.as_str()))
        .cloned()
        .collect();
    for it in items {
        let bytes = match source.read(&it.source_ref) {
            Ok(Some(b)) => b,
            Ok(None) => {
                reject(&mut rep, &it.source_id, "FILE_MISSING");
                continue;
            }
            Err(_) => {
                reject(&mut rep, &it.source_id, "FILE_READ_ERROR");
                continue;
            }
        };
        if bytes.is_empty() {
            reject(&mut rep, &it.source_id, "FILE_EMPTY");
            continue;
        }
        let sha = sha256_hex(&bytes);
        if it
            .expected_sha256
            .as_deref()
            .is_some_and(|e| !e.eq_ignore_ascii_case(&sha))
        {
            reject(&mut rep, &it.source_id, "SOURCE_HASH_MISMATCH");
            continue;
        }
        if it.expected_size.is_some_and(|s| s != bytes.len() as u64) {
            reject(&mut rep, &it.source_id, "SOURCE_SIZE_MISMATCH");
            continue;
        }
        // The record must exist before any bytes are copied, so a bad reference leaves no blob.
        let rec = match store.get(scope, &it.entity, &it.record_id) {
            Ok(r) => r,
            Err(StoreError::NotFound) => {
                reject(&mut rep, &it.source_id, "MISSING_RECORD");
                continue;
            }
            Err(_) => {
                reject(&mut rep, &it.source_id, "STORE_ERROR");
                continue;
            }
        };
        if !seen_hashes.insert(sha.clone()) {
            rep.duplicates += 1;
        }
        let key = object_key(scope, &it.entity, &it.record_id, &it.field, &sha);
        if blobs.put(scope, &key, &bytes, &it.content_type).is_err() {
            reject(&mut rep, &it.source_id, "COPY_FAILED");
            continue;
        }
        match blobs.get(scope, &key) {
            Ok(Some((back, _))) if sha256_hex(&back) == sha => {}
            Ok(Some(_)) => {
                reject(&mut rep, &it.source_id, "DEST_HASH_MISMATCH");
                continue;
            }
            Ok(None) => {
                reject(&mut rep, &it.source_id, "DEST_MISSING");
                continue;
            }
            Err(_) => {
                reject(&mut rep, &it.source_id, "COPY_FAILED");
                continue;
            }
        }
        let meta = json!({"key": key, "sha256": sha, "size": bytes.len(),
                          "content_type": it.content_type, "name": it.name});
        if rec.data.get(&it.field) != Some(&meta) {
            let mut data = rec.data.clone();
            let Some(obj) = data.as_object_mut() else {
                reject(&mut rep, &it.source_id, "LINK_FAILED");
                continue;
            };
            obj.insert(it.field.clone(), meta);
            if store
                .update(scope, &it.entity, &it.record_id, rec.version, data)
                .is_err()
            {
                reject(&mut rep, &it.source_id, "LINK_FAILED");
                continue;
            }
        }
        rep.verified.push(it.source_id.clone());
    }
    rep
}

/// A linked file that does not check out.
#[derive(Debug, PartialEq)]
pub struct FileFinding {
    pub entity: String,
    pub record_id: String,
    pub field: String,
    pub code: &'static str,
}

/// Re-checks files already linked: the key belongs to this tenant, record and field, the object
/// is there, and its bytes hash to what the key says. Reads only.
pub fn verify_files<S: Store>(
    store: &S,
    blobs: &dyn BlobStore,
    scope: &TenantScope,
    refs: &[(String, String, String)],
) -> Vec<FileFinding> {
    let mut out = Vec::new();
    for (entity, id, field) in refs {
        let mut find = |code: &'static str| {
            out.push(FileFinding {
                entity: entity.clone(),
                record_id: id.clone(),
                field: field.clone(),
                code,
            })
        };
        let Ok(rec) = store.get(scope, entity, id) else {
            find("MISSING_RECORD");
            continue;
        };
        let Some(key) = rec
            .data
            .get(field)
            .and_then(Value::as_object)
            .and_then(|m| m.get("key"))
            .and_then(Value::as_str)
        else {
            find("NOT_LINKED");
            continue;
        };
        let own = format!("{}/{entity}/{id}/{field}/", scope.tenant_id());
        if !key.starts_with(&own) {
            let tenant_part = key.split('/').next().unwrap_or_default();
            find(if tenant_part != scope.tenant_id() {
                "CROSS_TENANT"
            } else {
                "WRONG_OWNER"
            });
            continue;
        }
        let want = key.rsplit('/').next().unwrap_or_default();
        match blobs.get(scope, key) {
            Ok(Some((b, _))) if sha256_hex(&b) == want => {}
            Ok(Some(_)) => find("DEST_HASH_MISMATCH"),
            _ => find("DEST_MISSING"),
        }
    }
    out
}
