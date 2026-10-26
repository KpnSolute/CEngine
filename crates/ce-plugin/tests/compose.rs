use ce_plugin::{builtin_registry, compose, parse, ComposeError, Plugin};
use semver::Version;
use serde_json::{json, Value};

const ENGINE: Version = Version::new(0, 1, 0);

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn shell() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "entities": {
            "period": {
                "fields": {
                    "label": {"type": "string", "required": true, "unique": true, "max_length": 80, "indexed": true},
                    "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true},
                    "week_1_locked": {"type": "boolean", "default": false},
                    "week_2_locked": {"type": "boolean", "default": false},
                    "week_3_locked": {"type": "boolean", "default": false},
                    "week_4_locked": {"type": "boolean", "default": false}
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
            "below_par",
            "ending_qty",
            "ending_value",
            "on_hand",
            "opening_value",
            "over_pull",
            "price",
            "price_decision",
            "pulled_value",
            "received_value",
            "shortage"
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
    let with = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
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
    match compose(&shell(), "business.inventory", Some((&p, DIGEST))) {
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
    let mut no_flag = shell();
    no_flag["entities"]["period"]["fields"]
        .as_object_mut()
        .unwrap()
        .remove("week_3_locked");
    assert_eq!(
        compose(&no_flag, "business.inventory", None).unwrap_err(),
        ComposeError::MissingField("period".into(), "week_3_locked".into())
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
        compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::PluginMismatch
    );
    let mut p = plugin("");
    p.module_api = 2;
    assert_eq!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::PluginMismatch
    );
}

#[test]
fn the_composed_manifest_names_its_module_and_pinned_plugin() {
    let p = plugin("");
    let with = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    assert_eq!(
        with.manifest["provenance"],
        json!({
            "module": {"name": "business.inventory", "api": 1},
            "plugin": {"id": p.id, "version": p.version.to_string(), "digest": DIGEST}
        })
    );
    let bare = compose(&shell(), "business.inventory", None).unwrap();
    assert_eq!(
        bare.manifest["provenance"],
        json!({"module": {"name": "business.inventory", "api": 1}})
    );
    // A different pinned digest is a different manifest.
    let other = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let moved = compose(&shell(), "business.inventory", Some((&p, other))).unwrap();
    assert_ne!(with.hash, moved.hash);
}

#[test]
fn an_author_cannot_write_provenance_by_hand() {
    let mut forged = compose(&shell(), "business.inventory", None)
        .unwrap()
        .manifest;
    forged["provenance"]["plugin"] = json!({"id": "x", "version": "1.0.0", "digest": DIGEST});
    let issues = ce_manifest::validate(forged.to_string().as_bytes()).unwrap_err();
    assert!(issues.iter().any(|i| i.code == "provenance_reserved"));
    // Only the composing host may accept it.
    assert!(ce_manifest::validate_composed(forged.to_string().as_bytes()).is_ok());
}

#[test]
fn a_long_but_legal_plugin_version_still_composes() {
    let mut p = plugin("");
    p.version = semver::Version::parse(&format!("1.0.0-{}", "a".repeat(120))).unwrap();
    assert!(compose(&shell(), "business.inventory", Some((&p, DIGEST))).is_ok());
}

fn extending(entity: &str, field: &str, def: &str) -> String {
    format!(
        r#"[[extend]]
entity = "{entity}"
field = "{field}"
definition = {def}
"#
    )
}

#[test]
fn a_plugin_can_add_a_computed_field_to_a_module_entity() {
    let p = plugin(&extending(
        "stock_line",
        "short_value",
        r#"{ type = "decimal", precision = 16, scale = 2, computed = "when(ending_qty < 0, 0 - ending_qty * price, 0)" }"#,
    ));
    let with = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    let without = compose(&shell(), "business.inventory", Some((&plugin(""), DIGEST))).unwrap();
    assert_ne!(with.hash, without.hash);
    let f = &with.manifest["entities"]["stock_line"]["fields"]["short_value"];
    assert_eq!(f["type"], "decimal");
    assert!(without.manifest["entities"]["stock_line"]["fields"]["short_value"].is_null());
}

#[test]
fn an_added_field_cannot_replace_one_the_module_has() {
    let p = plugin(&extending(
        "stock_line",
        "ending_qty",
        r#"{ type = "string" }"#,
    ));
    assert_eq!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::Clash("stock_line.ending_qty".into())
    );
}

#[test]
fn an_added_field_must_go_on_an_entity_the_module_added() {
    let p = plugin(&extending("period", "note", r#"{ type = "string" }"#));
    assert_eq!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::MissingEntity("period".into())
    );
}

#[test]
fn an_added_expression_is_checked_when_composed() {
    let p = plugin(&extending(
        "stock_line",
        "bad",
        r#"{ type = "decimal", precision = 16, scale = 2, computed = "nope + 1" }"#,
    ));
    assert!(matches!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))),
        Err(ComposeError::Invalid(_))
    ));
}

#[test]
fn bad_added_field_names_and_duplicates_are_refused_when_parsed() {
    let text = |extra: &str| {
        format!(
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
{extra}"#
        )
    };
    let bad = parse(
        &text(&extending(
            "stock_line",
            "Bad-Name",
            r#"{ type = "string" }"#,
        )),
        &builtin_registry(),
        &ENGINE,
    )
    .unwrap_err();
    assert!(bad.iter().any(|v| v.code == "bad_field_name"));
    let twice = format!(
        "{}{}",
        extending("stock_line", "x", r#"{ type = "string" }"#),
        extending("stock_line", "x", r#"{ type = "string" }"#)
    );
    let dup = parse(&text(&twice), &builtin_registry(), &ENGINE).unwrap_err();
    assert!(dup.iter().any(|v| v.code == "duplicate_field"));
}
