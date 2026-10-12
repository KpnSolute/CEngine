use ce_plugin::{builtin_registry, compose, parse, ComposeError, Plugin};
use semver::Version;
use serde_json::{json, Value};

const ENGINE: Version = Version::new(0, 1, 0);

fn shell() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "entities": {
            "period": {
                "fields": {
                    "label": {"type": "string", "required": true, "unique": true, "max_length": 80, "indexed": true},
                    "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true}
                },
                "sort_keys": ["label"]
            }
        },
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    })
}

fn plugin(slots: &str) -> Plugin {
    let text = format!(
        r#"schema_version = 1
[plugin]
id = "kitchen-rules"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
{slots}"#
    );
    parse(&text, &builtin_registry(), &ENGINE).unwrap_or_else(|e| panic!("{e:?}"))
}

#[test]
fn the_registry_lists_the_inventory_module_and_its_slots() {
    let reg = builtin_registry();
    let m = reg.module("business.inventory").unwrap();
    assert_eq!(m.api, 1);
    let mut slots: Vec<&str> = m.slots.iter().map(|s| s.name.as_str()).collect();
    slots.sort();
    assert_eq!(
        slots,
        [
            "ending_qty",
            "ending_value",
            "opening_value",
            "over_pull",
            "price",
            "pulled_value",
            "received_value"
        ]
    );
}

#[test]
fn the_module_composes_with_its_defaults() {
    let c = compose(&shell(), "business.inventory", None).unwrap();
    let line = &c.manifest["entities"]["stock_line"];
    assert_eq!(
        line["fields"]["ending_qty"]["computed"],
        "max(0, opening_oh + received - pulled)"
    );
    assert_eq!(
        line["invariants"][1]["expr"],
        "opening_oh + received - pulled >= 0"
    );
    assert_eq!(
        c.manifest["roles"],
        shell()["roles"],
        "roles stay the template's"
    );
    assert!(!c.manifest.to_string().contains("$slot"));
}

#[test]
fn a_plugin_changes_only_the_slots_it_fills() {
    let p = plugin(
        r#"[[policy]]
slot = "ending_qty"
rule = "opening_oh + received - pulled"
[[policy]]
slot = "over_pull"
rule = "true"
"#,
    );
    let with = compose(&shell(), "business.inventory", Some(&p)).unwrap();
    let without = compose(&shell(), "business.inventory", None).unwrap();
    assert_ne!(with.hash, without.hash);
    let f = &with.manifest["entities"]["stock_line"];
    assert_eq!(
        f["fields"]["ending_qty"]["computed"],
        "opening_oh + received - pulled"
    );
    assert_eq!(f["invariants"][1]["expr"], "true");
    assert_eq!(
        f["fields"]["price"],
        without.manifest["entities"]["stock_line"]["fields"]["price"]
    );
}

#[test]
fn composition_is_deterministic() {
    let a = compose(&shell(), "business.inventory", None).unwrap();
    let b = compose(&shell(), "business.inventory", None).unwrap();
    assert_eq!(a.hash, b.hash);
}

#[test]
fn a_rule_that_does_not_type_check_is_refused_at_install() {
    let p = plugin(
        r#"[[policy]]
slot = "ending_qty"
rule = "opening_oh + no_such_field"
"#,
    );
    match compose(&shell(), "business.inventory", Some(&p)) {
        Err(ComposeError::Invalid(issues)) => assert!(!issues.is_empty()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn composition_refuses_bad_setups() {
    let mut no_period = shell();
    no_period["entities"]
        .as_object_mut()
        .unwrap()
        .remove("period");
    assert_eq!(
        compose(&no_period, "business.inventory", None).unwrap_err(),
        ComposeError::MissingEntity("period".into())
    );
    let mut clash = shell();
    clash["entities"]["stock_item"] = json!({"fields": {"x": {"type": "string"}}});
    assert_eq!(
        compose(&clash, "business.inventory", None).unwrap_err(),
        ComposeError::Clash("stock_item".into())
    );
    assert_eq!(
        compose(&shell(), "business.payroll", None).unwrap_err(),
        ComposeError::UnknownModule("business.payroll".into())
    );
    assert_eq!(
        compose(&json!([]), "business.inventory", None).unwrap_err(),
        ComposeError::NotAnObject
    );
}

#[test]
fn a_plugin_for_another_module_is_refused() {
    let mut p = plugin("");
    p.module = "business.other".into();
    assert_eq!(
        compose(&shell(), "business.inventory", Some(&p)).unwrap_err(),
        ComposeError::PluginMismatch
    );
    let mut p = plugin("");
    p.module_api = 2;
    assert_eq!(
        compose(&shell(), "business.inventory", Some(&p)).unwrap_err(),
        ComposeError::PluginMismatch
    );
}
