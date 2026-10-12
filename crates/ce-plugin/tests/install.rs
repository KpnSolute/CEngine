//! Installing and resolving plugins: the approval record, and the manifest an installation
//! serves. Every failure refuses; none falls back to the defaults.
use ce_plugin::{digest_of, resolve, Installed, LoadError, Pin, ResolveError};
use semver::Version;
use serde_json::{json, Value};
use std::path::Path;

const ENGINE: Version = Version::new(0, 1, 0);

fn shell() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "entities": {"period": {"fields": {
            "label": {"type": "string", "required": true, "unique": true, "max_length": 80, "indexed": true},
            "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true},
            "week_1_locked": {"type": "boolean", "default": false},
            "week_2_locked": {"type": "boolean", "default": false},
            "week_3_locked": {"type": "boolean", "default": false},
            "week_4_locked": {"type": "boolean", "default": false}
        }, "sort_keys": ["label"]}},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    })
}

fn descriptor(version: &str, rule: &str) -> String {
    format!(
        r#"schema_version = 1
[plugin]
id = "kitchen-rules"
version = "{version}"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
[[policy]]
slot = "ending_qty"
rule = "{rule}"
"#
    )
}

fn write(root: &Path, text: &str) {
    let dir = root.join(".cengine/plugins/kitchen-rules");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), text).unwrap();
}

fn approve(inst: &mut Installed, root: &Path, version: &str) -> String {
    let digest = digest_of(root, "kitchen-rules").unwrap();
    inst.approve(Pin {
        id: "kitchen-rules".into(),
        version: Version::parse(version).unwrap(),
        digest: digest.clone(),
    });
    digest
}

fn go(root: &Path, inst: &Installed) -> Result<ce_plugin::Composed, ResolveError> {
    resolve(
        &shell(),
        "business.inventory",
        Some("kitchen-rules"),
        root,
        inst,
        &ENGINE,
    )
}

const LENIENT: &str = "opening_oh + received - pulled";

#[test]
fn an_approved_plugin_is_served_with_its_rules_and_digest() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    let d = approve(&mut inst, dir.path(), "0.1.0");
    let c = go(dir.path(), &inst).unwrap();
    assert_eq!(
        c.manifest["entities"]["stock_line"]["fields"]["ending_qty"]["computed"],
        LENIENT
    );
    assert_eq!(c.manifest["provenance"]["plugin"]["digest"], d);
}

#[test]
fn no_plugin_means_the_module_defaults() {
    let c = resolve(
        &shell(),
        "business.inventory",
        None,
        Path::new("/nonexistent"),
        &Installed::new(),
        &ENGINE,
    )
    .unwrap();
    assert!(c.manifest["provenance"].get("plugin").is_none());
}

#[test]
fn a_plugin_nobody_approved_is_refused_not_defaulted() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    assert!(matches!(
        go(dir.path(), &Installed::new()),
        Err(ResolveError::NotApproved(_))
    ));
}

#[test]
fn a_disabled_plugin_is_refused_until_enabled() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    inst.disable("kitchen-rules");
    assert!(matches!(
        go(dir.path(), &inst),
        Err(ResolveError::NotApproved(_))
    ));
    inst.enable("kitchen-rules");
    assert!(go(dir.path(), &inst).is_ok());
}

#[test]
fn a_file_edited_after_approval_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    write(dir.path(), &descriptor("0.1.0", "opening_oh"));
    assert!(matches!(
        go(dir.path(), &inst),
        Err(ResolveError::Load(LoadError::DigestMismatch { .. }))
    ));
}

#[test]
fn a_missing_plugin_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    std::fs::remove_dir_all(dir.path().join(".cengine")).unwrap();
    assert!(matches!(
        go(dir.path(), &inst),
        Err(ResolveError::Load(LoadError::NotFound))
    ));
}

#[test]
fn a_plugin_that_does_not_type_check_is_refused_at_install() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &descriptor("0.1.0", "opening_oh + no_such_field"),
    );
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    assert!(matches!(
        go(dir.path(), &inst),
        Err(ResolveError::Compose(_))
    ));
}

#[test]
fn rollback_serves_the_older_approval_only_when_its_bytes_are_present() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    let d1 = approve(&mut inst, dir.path(), "0.1.0");
    write(dir.path(), &descriptor("0.2.0", "opening_oh"));
    let d2 = approve(&mut inst, dir.path(), "0.2.0");
    assert_eq!(
        go(dir.path(), &inst).unwrap().manifest["provenance"]["plugin"]["digest"],
        d2
    );
    inst.rollback("kitchen-rules").unwrap();
    // The folder still holds 0.2.0, which is no longer approved.
    assert!(matches!(
        go(dir.path(), &inst),
        Err(ResolveError::Load(LoadError::DigestMismatch { .. }))
    ));
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    assert_eq!(
        go(dir.path(), &inst).unwrap().manifest["provenance"]["plugin"]["digest"],
        d1
    );
}

#[test]
fn the_record_round_trips_with_its_history() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    write(dir.path(), &descriptor("0.2.0", "opening_oh"));
    approve(&mut inst, dir.path(), "0.2.0");
    inst.rollback("kitchen-rules").unwrap();
    inst.disable("kitchen-rules");
    let text = inst.to_json();
    let back = Installed::from_json(&text).unwrap();
    assert_eq!(back.to_json(), text);
    assert_eq!(back.history("kitchen-rules").len(), 2);
    assert!(back.is_disabled("kitchen-rules"));
    assert!(back.active("kitchen-rules").is_none());
}

#[test]
fn a_damaged_or_hand_edited_record_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    let good: Value = serde_json::from_str(&inst.to_json()).unwrap();
    let bad = |f: &dyn Fn(&mut Value)| {
        let mut v = good.clone();
        f(&mut v);
        assert!(Installed::from_json(&v.to_string()).is_err(), "{v}");
    };
    bad(&|v| v["extra"] = json!(1));
    bad(&|v| v["version"] = json!(2));
    bad(&|v| v["plugins"]["kitchen-rules"]["active"] = json!([5]));
    bad(&|v| v["plugins"]["kitchen-rules"]["active"] = json!([]));
    bad(&|v| v["plugins"]["kitchen-rules"]["log"][0]["digest"] = json!("sha256:XYZ"));
    bad(&|v| v["plugins"]["kitchen-rules"]["log"][0]["version"] = json!("one"));
    bad(&|v| v["plugins"]["kitchen-rules"]["x"] = json!(1));
    bad(&|v| {
        let e = v["plugins"]["kitchen-rules"].clone();
        v["plugins"]["../etc"] = e;
    });
    assert!(Installed::from_json("not json").is_err());
}

#[test]
fn an_active_history_that_goes_backwards_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), &descriptor("0.1.0", LENIENT));
    let mut inst = Installed::new();
    approve(&mut inst, dir.path(), "0.1.0");
    write(dir.path(), &descriptor("0.2.0", "opening_oh"));
    approve(&mut inst, dir.path(), "0.2.0");
    let mut v: Value = serde_json::from_str(&inst.to_json()).unwrap();
    v["plugins"]["kitchen-rules"]["active"] = json!([1, 0]);
    assert!(Installed::from_json(&v.to_string()).is_err());
    v["plugins"]["kitchen-rules"]["active"] = json!([0, 0]);
    assert!(Installed::from_json(&v.to_string()).is_err());
    v["plugins"]["kitchen-rules"]["active"] = json!([0, 1]);
    assert!(Installed::from_json(&v.to_string()).is_ok());
}
