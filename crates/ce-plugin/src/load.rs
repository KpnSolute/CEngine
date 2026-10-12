//! Digest-pinned loading. A plugin is loaded from a local folder, never from the network, and
//! only when the bytes on disk hash to the digest the installation approved.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

use crate::{parse, Plugin, Registry, Violation, MAX_BYTES};

/// Where plugins live inside a workspace checkout.
pub const PLUGINS_DIR: &str = ".cengine/plugins";
pub const DESCRIPTOR: &str = "plugin.toml";

/// What an installation approved: one exact plugin version and the digest of its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub id: String,
    pub version: Version,
    /// `sha256:` followed by 64 lowercase hex characters.
    pub digest: String,
}

#[derive(Debug)]
pub enum LoadError {
    /// The slug is not a safe folder name.
    BadSlug,
    /// No regular `plugin.toml` at the expected place (missing, a symlink, or not a file).
    NotFound,
    TooLarge,
    Io(String),
    /// Bytes on disk do not hash to the pinned digest.
    DigestMismatch {
        pinned: String,
        found: String,
    },
    /// Id or version inside the file disagrees with the pin or the folder.
    IdentityMismatch(&'static str),
    Invalid(Vec<Violation>),
}

/// `sha256:<hex>` of the exact descriptor bytes.
pub fn digest(bytes: &[u8]) -> String {
    let h = Sha256::digest(bytes);
    let mut out = String::with_capacity(7 + 64);
    out.push_str("sha256:");
    for b in h {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn safe_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn descriptor_path(root: &Path, slug: &str) -> PathBuf {
    root.join(PLUGINS_DIR).join(slug).join(DESCRIPTOR)
}

/// A real directory, not a symlink to one.
fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

/// Read the descriptor bytes: the file must be a regular file (not a symlink) under real
/// directories, and no larger than the descriptor limit.
fn read_descriptor(root: &Path, slug: &str) -> Result<Vec<u8>, LoadError> {
    if !safe_slug(slug) {
        return Err(LoadError::BadSlug);
    }
    let plugins = root.join(PLUGINS_DIR);
    let dir = plugins.join(slug);
    if !is_real_dir(&root.join(".cengine")) || !is_real_dir(&plugins) || !is_real_dir(&dir) {
        return Err(LoadError::NotFound);
    }
    let path = descriptor_path(root, slug);
    let meta = fs::symlink_metadata(&path).map_err(|_| LoadError::NotFound)?;
    if !meta.is_file() {
        return Err(LoadError::NotFound);
    }
    if meta.len() > MAX_BYTES as u64 {
        return Err(LoadError::TooLarge);
    }
    let mut buf = Vec::new();
    fs::File::open(&path)
        .and_then(|f| f.take(MAX_BYTES as u64 + 1).read_to_end(&mut buf))
        .map_err(|e| LoadError::Io(e.kind().to_string()))?;
    if buf.len() > MAX_BYTES {
        return Err(LoadError::TooLarge);
    }
    Ok(buf)
}

/// Slugs of plugin folders under `<root>/.cengine/plugins`, sorted. Only real directories with
/// a safe name are listed; nothing is read or executed.
pub fn discover(root: &Path) -> Vec<String> {
    let plugins = root.join(PLUGINS_DIR);
    if !is_real_dir(&root.join(".cengine")) || !is_real_dir(&plugins) {
        return Vec::new();
    }
    let mut out: Vec<String> = fs::read_dir(&plugins)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| is_real_dir(&e.path()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| safe_slug(n))
        .collect();
    out.sort();
    out
}

/// Digest of what is on disk now, for an approver to pin. Does not validate the contents.
pub fn digest_of(root: &Path, slug: &str) -> Result<String, LoadError> {
    read_descriptor(root, slug).map(|b| digest(&b))
}

/// Load a plugin only if its bytes match `pin`, then validate it. The digest is checked on the
/// same bytes that are parsed, so the file cannot change between check and use.
pub fn load(
    root: &Path,
    slug: &str,
    pin: &Pin,
    registry: &Registry,
    engine: &Version,
) -> Result<Plugin, LoadError> {
    let bytes = read_descriptor(root, slug)?;
    let found = digest(&bytes);
    if found != pin.digest {
        return Err(LoadError::DigestMismatch {
            pinned: pin.digest.clone(),
            found,
        });
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        LoadError::Invalid(vec![Violation {
            code: "malformed",
            path: String::new(),
            message: "not UTF-8".into(),
        }])
    })?;
    let plugin = parse(text, registry, engine).map_err(LoadError::Invalid)?;
    if plugin.id != slug {
        return Err(LoadError::IdentityMismatch(
            "folder name differs from plugin.id",
        ));
    }
    if plugin.id != pin.id {
        return Err(LoadError::IdentityMismatch(
            "plugin.id differs from the pin",
        ));
    }
    if plugin.version != pin.version {
        return Err(LoadError::IdentityMismatch(
            "plugin.version differs from the pin",
        ));
    }
    Ok(plugin)
}

/// The plugins approved for one workspace installation, with the history needed to roll back.
/// Pure data: persistence belongs to the host store.
#[derive(Debug, Clone, Default)]
pub struct Installed {
    /// Approved pins per plugin id, oldest first; the last is active.
    history: std::collections::BTreeMap<String, Vec<Pin>>,
    disabled: std::collections::BTreeSet<String>,
}

impl Installed {
    pub fn new() -> Self {
        Self::default()
    }

    /// Approve a pin as the active version of its plugin. Re-approving the active pin is a no-op.
    pub fn approve(&mut self, pin: Pin) {
        let h = self.history.entry(pin.id.clone()).or_default();
        if h.last() != Some(&pin) {
            h.push(pin);
        }
    }

    /// The active pin, or `None` when the plugin is unknown or disabled.
    pub fn active(&self, id: &str) -> Option<&Pin> {
        if self.disabled.contains(id) {
            return None;
        }
        self.history.get(id).and_then(|h| h.last())
    }

    /// Drop the active pin and return to the previous one. Fails when there is nothing to go
    /// back to, so a plugin is never left without an approved version by rollback.
    pub fn rollback(&mut self, id: &str) -> Option<&Pin> {
        let h = self.history.get_mut(id)?;
        if h.len() < 2 {
            return None;
        }
        h.pop();
        h.last()
    }

    /// Stop serving a plugin without forgetting its history.
    pub fn disable(&mut self, id: &str) {
        if self.history.contains_key(id) {
            self.disabled.insert(id.to_string());
        }
    }

    pub fn enable(&mut self, id: &str) {
        self.disabled.remove(id);
    }

    /// Every pin ever approved for a plugin, oldest first.
    pub fn history(&self, id: &str) -> &[Pin] {
        self.history.get(id).map(Vec::as_slice).unwrap_or(&[])
    }
}
