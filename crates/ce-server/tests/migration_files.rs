//! G7: legacy files are copied with proof. Source hash, copy, read-back hash, then the link;
//! every failure carries a code and no content. Needs no database.
use ce_auth::{TenantScope, Verifier};
use ce_server::files_migrate::{migrate_files, verify_files, FileItem, SourceFiles};
use ce_server::{BlobStore, MemoryBlobs};
use ce_store::{MemoryStore, Store};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const T1: &str = "11111111-1111-1111-1111-111111111111";
const T2: &str = "22222222-2222-2222-2222-222222222222";

fn scope(t: &str) -> TenantScope {
    Verifier::new(b"0123456789abcdef0123456789abcdef")
        .unwrap()
        .public_scope(t)
        .unwrap()
}

fn hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

struct Src(HashMap<String, Vec<u8>>);
impl SourceFiles for Src {
    fn read(&self, r: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.get(r).cloned())
    }
}

/// A blob store that damages every object it is given.
struct Corrupting(MemoryBlobs);
impl BlobStore for Corrupting {
    fn put(&self, s: &TenantScope, k: &str, b: &[u8], ct: &str) -> Result<(), String> {
        let mut bad = b.to_vec();
        bad[0] ^= 0xff;
        self.0.put(s, k, &bad, ct)
    }
    fn get(&self, s: &TenantScope, k: &str) -> Result<Option<(Vec<u8>, String)>, String> {
        self.0.get(s, k)
    }
}

fn item(src: &str, rec: &str, bytes: &[u8]) -> FileItem {
    FileItem {
        source_id: format!("legacy-{src}"),
        source_ref: src.to_string(),
        entity: "supplier_invoice".into(),
        record_id: rec.to_string(),
        field: "scan".into(),
        name: "invoice.pdf".into(),
        content_type: "application/pdf".into(),
        expected_sha256: Some(hex(bytes)),
        expected_size: Some(bytes.len() as u64),
    }
}

fn rec(st: &MemoryStore, s: &TenantScope) -> String {
    st.insert(s, "supplier_invoice", json!({"number": "A-1"}), None)
        .unwrap()
        .id
}

#[test]
fn files_copy_read_back_and_link() {
    let (st, blobs, s) = (MemoryStore::new(), MemoryBlobs::new(), scope(T1));
    let (a, b) = (rec(&st, &s), rec(&st, &s));
    let src = Src(HashMap::from([
        ("f/a.pdf".to_string(), b"first file".to_vec()),
        ("f/b.pdf".to_string(), b"second file".to_vec()),
    ]));
    let items = vec![
        item("f/a.pdf", &a, b"first file"),
        item("f/b.pdf", &b, b"second file"),
    ];
    let all = vec!["f/a.pdf".to_string(), "f/b.pdf".to_string()];
    let r = migrate_files(&st, &blobs, &src, &s, &items, &all);
    assert_eq!(r.verified.len(), 2, "{:?}", r.rejected);
    assert!(r.rejected.is_empty() && r.orphan_sources.is_empty());
    assert_eq!(blobs.len(), 2);
    let linked = st.get(&s, "supplier_invoice", &a).unwrap();
    assert_eq!(linked.data["scan"]["sha256"], hex(b"first file"));
    assert!(linked.data["scan"]["key"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{T1}/supplier_invoice/{a}/scan/")));
    let refs = vec![
        ("supplier_invoice".to_string(), a, "scan".to_string()),
        ("supplier_invoice".to_string(), b, "scan".to_string()),
    ];
    assert!(verify_files(&st, &blobs, &s, &refs).is_empty());
}

#[test]
fn every_bad_file_is_named_by_code_and_leaves_no_blob() {
    let (st, blobs, s) = (MemoryStore::new(), MemoryBlobs::new(), scope(T1));
    let r1 = rec(&st, &s);
    let src = Src(HashMap::from([
        ("ok".to_string(), b"fine".to_vec()),
        ("changed".to_string(), b"not what was recorded".to_vec()),
        ("short".to_string(), b"abc".to_vec()),
        ("empty".to_string(), Vec::new()),
        ("norec".to_string(), b"has no record".to_vec()),
    ]));
    let mut short = item("short", &r1, b"abc");
    short.expected_size = Some(99);
    let items = vec![
        item("ok", &r1, b"fine"),
        item("gone", &r1, b"never there"),
        item("changed", &r1, b"what was recorded"),
        short,
        item("empty", &r1, b"x"),
        item("norec", "no-such-record", b"has no record"),
    ];
    let r = migrate_files(&st, &blobs, &src, &s, &items, &[]);
    let codes: Vec<_> = r.rejected.iter().map(|(_, c)| *c).collect();
    assert_eq!(
        codes,
        [
            "FILE_MISSING",
            "SOURCE_HASH_MISMATCH",
            "SOURCE_SIZE_MISMATCH",
            "FILE_EMPTY",
            "MISSING_RECORD"
        ]
    );
    assert_eq!(r.verified, ["legacy-ok"]);
    assert_eq!(
        blobs.len(),
        1,
        "a rejected file must not leave bytes behind"
    );
}

#[test]
fn a_copy_that_does_not_hash_back_is_rejected_and_not_linked() {
    let (st, s) = (MemoryStore::new(), scope(T1));
    let blobs = Corrupting(MemoryBlobs::new());
    let a = rec(&st, &s);
    let src = Src(HashMap::from([("f".to_string(), b"payload".to_vec())]));
    let r = migrate_files(&st, &blobs, &src, &s, &[item("f", &a, b"payload")], &[]);
    assert_eq!(r.rejected, [("legacy-f".to_string(), "DEST_HASH_MISMATCH")]);
    assert!(r.verified.is_empty());
    assert!(st
        .get(&s, "supplier_invoice", &a)
        .unwrap()
        .data
        .get("scan")
        .is_none());
}

#[test]
fn duplicates_are_counted_and_orphans_listed() {
    let (st, blobs, s) = (MemoryStore::new(), MemoryBlobs::new(), scope(T1));
    let (a, b) = (rec(&st, &s), rec(&st, &s));
    let src = Src(HashMap::from([
        ("one".to_string(), b"same bytes".to_vec()),
        ("two".to_string(), b"same bytes".to_vec()),
    ]));
    let all = vec!["one".to_string(), "two".to_string(), "stray".to_string()];
    let r = migrate_files(
        &st,
        &blobs,
        &src,
        &s,
        &[
            item("one", &a, b"same bytes"),
            item("two", &b, b"same bytes"),
        ],
        &all,
    );
    assert_eq!((r.verified.len(), r.duplicates), (2, 1));
    assert_eq!(r.orphan_sources, ["stray"]);
}

#[test]
fn running_twice_changes_nothing() {
    let (st, blobs, s) = (MemoryStore::new(), MemoryBlobs::new(), scope(T1));
    let a = rec(&st, &s);
    let src = Src(HashMap::from([("f".to_string(), b"payload".to_vec())]));
    let items = vec![item("f", &a, b"payload")];
    migrate_files(&st, &blobs, &src, &s, &items, &[]);
    let v1 = st.get(&s, "supplier_invoice", &a).unwrap().version;
    let r = migrate_files(&st, &blobs, &src, &s, &items, &[]);
    assert_eq!(r.verified.len(), 1);
    assert_eq!(blobs.len(), 1);
    assert_eq!(st.get(&s, "supplier_invoice", &a).unwrap().version, v1);
}

#[test]
fn verify_finds_missing_tampered_cross_tenant_and_misowned_links() {
    let (st, blobs, s) = (MemoryStore::new(), MemoryBlobs::new(), scope(T1));
    let ids: Vec<String> = (0..5).map(|_| rec(&st, &s)).collect();
    let good = hex(b"good");
    let link = |id: &str, key: String| {
        let mut d = st.get(&s, "supplier_invoice", id).unwrap();
        d.data["scan"] = json!({"key": key, "sha256": good});
        st.update(&s, "supplier_invoice", id, d.version, d.data)
            .unwrap();
    };
    let own = |id: &str, sha: &str| format!("{T1}/supplier_invoice/{id}/scan/{sha}");
    // 0: fine. 1: object missing. 2: bytes differ from the hash in the key.
    blobs.put(&s, &own(&ids[0], &good), b"good", "x").unwrap();
    link(&ids[0], own(&ids[0], &good));
    link(&ids[1], own(&ids[1], &good));
    blobs
        .put(&s, &own(&ids[2], &good), b"tampered", "x")
        .unwrap();
    link(&ids[2], own(&ids[2], &good));
    // 3: key under another tenant. 4: key that belongs to record 0.
    link(
        &ids[3],
        format!("{T2}/supplier_invoice/{}/scan/{good}", ids[3]),
    );
    link(&ids[4], own(&ids[0], &good));
    let refs: Vec<_> = ids
        .iter()
        .map(|i| {
            (
                "supplier_invoice".to_string(),
                i.clone(),
                "scan".to_string(),
            )
        })
        .collect();
    let codes: Vec<_> = verify_files(&st, &blobs, &s, &refs)
        .into_iter()
        .map(|f| f.code)
        .collect();
    assert_eq!(
        codes,
        [
            "DEST_MISSING",
            "DEST_HASH_MISMATCH",
            "CROSS_TENANT",
            "WRONG_OWNER"
        ]
    );
}
