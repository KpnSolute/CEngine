use std::fs;
use std::path::Path;

use ce_plugin::{
    digest, digest_of, discover, load, Installed, LoadError, Module, Pin, Registry, Slot,
};
use semver::Version;

const ENGINE: Version = Version::new(0, 2, 0);

fn registry() -> Registry {
    Registry::new().with(Module {
        id: "business.inventory".into(),
        api: 1,
        slots: vec![Slot {
            name: "valuation".into(),
            rule: Some(true),
            params: vec![],
        }],
        read: vec![],
        write: vec![],
        emit: vec![],
    })
}

fn descriptor(id: &str, version: &str) -> String {
    format!(
        r#"schema_version = 1
[plugin]
id = "{id}"
version = "{version}"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
[[policy]]
slot = "valuation"
rule = "round(qty * price, 2)"
"#
    )
}

fn write(root: &Path, slug: &str, text: &str) {
    let dir = root.join(".cengine/plugins").join(slug);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plugin.toml"), text).unwrap();
}

fn pin_of(text: &str, id: &str, version: &str) -> Pin {
    Pin {
        id: id.into(),
        version: Version::parse(version).unwrap(),
        digest: digest(text.as_bytes()),
    }
}

#[test]
fn loads_when_the_digest_matches() {
    let dir = tempfile::tempdir().unwrap();
    let text = descriptor("inventory-rules", "0.1.0");
    write(dir.path(), "inventory-rules", &text);
    let pin = pin_of(&text, "inventory-rules", "0.1.0");
    assert_eq!(
        digest_of(dir.path(), "inventory-rules").unwrap(),
        pin.digest
    );
    let plugin = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap();
    assert_eq!(plugin.id, "inventory-rules");
}

#[test]
fn a_tampered_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let text = descriptor("inventory-rules", "0.1.0");
    write(dir.path(), "inventory-rules", &text);
    let pin = pin_of(&text, "inventory-rules", "0.1.0");
    write(
        dir.path(),
        "inventory-rules",
        &text.replace("round(qty * price, 2)", "qty * price"),
    );
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::DigestMismatch { .. }), "{err:?}");
}

#[test]
fn identity_must_match_folder_and_pin() {
    let dir = tempfile::tempdir().unwrap();
    let text = descriptor("other-name", "0.1.0");
    write(dir.path(), "inventory-rules", &text);
    let pin = pin_of(&text, "other-name", "0.1.0");
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::IdentityMismatch(_)), "{err:?}");

    let text = descriptor("inventory-rules", "0.2.0");
    write(dir.path(), "inventory-rules", &text);
    let pin = pin_of(&text, "inventory-rules", "0.1.0");
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::IdentityMismatch(_)), "{err:?}");
}

#[test]
fn an_invalid_but_pinned_file_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let text = descriptor("inventory-rules", "0.1.0").replace("policy-pack", "isolated-python");
    write(dir.path(), "inventory-rules", &text);
    let pin = pin_of(&text, "inventory-rules", "0.1.0");
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::Invalid(_)), "{err:?}");
}

#[test]
fn unsafe_slugs_and_missing_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let pin = pin_of("x", "x", "0.1.0");
    for slug in ["../etc", "a/b", "", "Upper", ".hidden", "a\\b"] {
        let err = load(dir.path(), slug, &pin, &registry(), &ENGINE).unwrap_err();
        assert!(matches!(err, LoadError::BadSlug), "{slug}: {err:?}");
    }
    let err = load(dir.path(), "absent", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::NotFound), "{err:?}");
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let text = descriptor("inventory-rules", "0.1.0");
    let pin = pin_of(&text, "inventory-rules", "0.1.0");

    // descriptor is a symlink to a file outside the workspace
    fs::write(outside.path().join("plugin.toml"), &text).unwrap();
    let folder = dir.path().join(".cengine/plugins/inventory-rules");
    fs::create_dir_all(&folder).unwrap();
    symlink(
        outside.path().join("plugin.toml"),
        folder.join("plugin.toml"),
    )
    .unwrap();
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::NotFound), "{err:?}");

    // the plugin folder itself is a symlink
    let dir2 = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir2.path().join(".cengine/plugins")).unwrap();
    symlink(
        outside.path(),
        dir2.path().join(".cengine/plugins/inventory-rules"),
    )
    .unwrap();
    let err = load(dir2.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::NotFound), "{err:?}");
    assert!(discover(dir2.path()).is_empty());
}

#[test]
fn oversize_descriptors_are_refused_before_hashing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "inventory-rules", &"#".repeat(70_000));
    let pin = pin_of("x", "inventory-rules", "0.1.0");
    let err = load(dir.path(), "inventory-rules", &pin, &registry(), &ENGINE).unwrap_err();
    assert!(matches!(err, LoadError::TooLarge), "{err:?}");
}

#[test]
fn discovery_lists_only_safe_plugin_folders() {
    let dir = tempfile::tempdir().unwrap();
    assert!(discover(dir.path()).is_empty());
    write(dir.path(), "b-rules", "x");
    write(dir.path(), "a-rules", "x");
    fs::create_dir_all(dir.path().join(".cengine/plugins/Bad_Name")).unwrap();
    fs::write(dir.path().join(".cengine/plugins/stray-file"), "x").unwrap();
    assert_eq!(discover(dir.path()), vec!["a-rules", "b-rules"]);
}

#[test]
fn upgrade_rollback_and_disable_keep_history() {
    let a = pin_of("a", "inventory-rules", "0.1.0");
    let b = pin_of("b", "inventory-rules", "0.2.0");
    let mut inst = Installed::new();
    assert!(inst.active("inventory-rules").is_none());
    inst.approve(a.clone());
    assert!(
        inst.rollback("inventory-rules").is_none(),
        "cannot roll back the only version"
    );
    inst.approve(b.clone());
    inst.approve(b.clone());
    assert_eq!(inst.history("inventory-rules").len(), 2);
    assert_eq!(inst.active("inventory-rules"), Some(&b));

    inst.disable("inventory-rules");
    assert!(inst.active("inventory-rules").is_none());
    inst.enable("inventory-rules");
    assert_eq!(inst.rollback("inventory-rules"), Some(&a));
    assert_eq!(inst.active("inventory-rules"), Some(&a));
    assert_eq!(
        inst.history("inventory-rules"),
        &[a.clone(), b.clone()],
        "rollback keeps the approval log"
    );
    // approving the pin we rolled back from makes it active again without losing anything
    inst.approve(b.clone());
    assert_eq!(inst.active("inventory-rules"), Some(&b));
    assert_eq!(inst.history("inventory-rules").len(), 3);
    inst.disable("never-installed");
    assert!(inst.active("never-installed").is_none());
}

#[test]
fn the_digest_is_stable_and_prefixed() {
    assert_eq!(
        digest(b""),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
