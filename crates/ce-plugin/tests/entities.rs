//! Plugins can add their own entities (tables). Names are namespaced by plugin id, references
//! between a plugin's own entities are rewritten, and nothing here is raw SQL.

use ce_plugin::{builtin_registry, compose, parse, ComposeError, Plugin};
use semver::Version;
use serde_json::{json, Value};

const ENGINE: Version = Version::new(0, 1, 0);
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

const HEAD: &str = r#"schema_version = 1
[plugin]
id = "food-safety"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
"#;

const TWO: &str = r#"
[[entity]]
name = "limit"
[entity.definition.fields.label]
type = "string"
required = true
unique = true
max_length = 80
[entity.definition.fields.max_f]
type = "integer"

[[entity]]
name = "check"
[entity.definition.fields.limit]
type = "ref"
ref = "limit"
required = true
[entity.definition.fields.item]
type = "ref"
ref = "stock_item"
[entity.definition.fields.reading]
type = "integer"
"#;

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

fn try_parse(extra: &str) -> Result<Plugin, Vec<ce_plugin::Violation>> {
    parse(&format!("{HEAD}{extra}"), &builtin_registry(), &ENGINE)
}

fn codes(extra: &str) -> Vec<&'static str> {
    try_parse(extra)
        .unwrap_err()
        .into_iter()
        .map(|v| v.code)
        .collect()
}

#[test]
fn a_plugin_may_declare_entities_and_they_install_under_its_namespace() {
    let p = try_parse(TWO).unwrap();
    assert_eq!(p.entities.len(), 2);
    let c = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    let ents = c.manifest["entities"].as_object().unwrap();
    assert!(ents.contains_key("food_safety__limit"));
    assert!(ents.contains_key("food_safety__check"));
    assert!(!ents.contains_key("limit"));
    // A ref to the plugin's own entity is rewritten; a ref to a module entity is left alone.
    let f = &ents["food_safety__check"]["fields"];
    assert_eq!(f["limit"]["ref"], "food_safety__limit");
    assert_eq!(f["item"]["ref"], "stock_item");
    // The template's roles are untouched.
    assert_eq!(c.manifest["roles"]["owner"]["write"], json!(["*"]));
}

#[test]
fn the_same_plugin_composes_to_the_same_manifest_hash() {
    let p = try_parse(TWO).unwrap();
    let a = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    let b = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    assert_eq!(a.hash, b.hash);
    let none = compose(&shell(), "business.inventory", None).unwrap();
    assert_ne!(a.hash, none.hash);
}

#[test]
fn bad_entity_names_are_refused_with_the_field_path() {
    for name in ["Limit", "1limit", "a__b", "limit_", "", "has-dash"] {
        let toml = format!(
            "[[entity]]\nname = \"{name}\"\n[entity.definition.fields.x]\ntype = \"string\"\n"
        );
        assert!(
            codes(&toml).contains(&"bad_entity_name"),
            "{name:?} should be refused"
        );
    }
}

#[test]
fn a_duplicate_entity_is_refused() {
    let one = "[[entity]]\nname = \"limit\"\n[entity.definition.fields.x]\ntype = \"string\"\n";
    assert!(codes(&format!("{one}{one}")).contains(&"duplicate_entity"));
}

#[test]
fn too_many_entities_are_refused() {
    let mut t = String::new();
    for i in 0..17 {
        t.push_str(&format!(
            "[[entity]]\nname = \"e{i}\"\n[entity.definition.fields.x]\ntype = \"string\"\n"
        ));
    }
    assert!(codes(&t).contains(&"too_many"));
}

#[test]
fn floats_in_a_definition_are_refused() {
    let t = "[[entity]]\nname = \"limit\"\n[entity.definition.fields.x]\ntype = \"decimal\"\ndefault = 1.5\n";
    assert!(codes(t).contains(&"bad_entity"));
}

#[test]
fn an_entity_needs_a_definition_and_unknown_keys_are_refused() {
    assert!(try_parse("[[entity]]\nname = \"limit\"\n").is_err());
    let t = "[[entity]]\nname = \"limit\"\nsql = \"create table x()\"\n[entity.definition.fields.x]\ntype = \"string\"\n";
    assert!(codes(t).contains(&"malformed"));
}

#[test]
fn a_definition_that_does_not_validate_is_refused_at_compose_time() {
    let t = "[[entity]]\nname = \"limit\"\n[entity.definition.fields.x]\ntype = \"no_such_type\"\n";
    let p = try_parse(t).unwrap();
    assert!(matches!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))),
        Err(ComposeError::Invalid(_))
    ));
}

#[test]
fn a_ref_to_an_unknown_entity_is_refused_at_compose_time() {
    let t = "[[entity]]\nname = \"check\"\n[entity.definition.fields.who]\ntype = \"ref\"\nref = \"nobody\"\n";
    let p = try_parse(t).unwrap();
    assert!(matches!(
        compose(&shell(), "business.inventory", Some((&p, DIGEST))),
        Err(ComposeError::Invalid(_))
    ));
}

#[test]
fn a_plugin_cannot_overwrite_a_template_entity() {
    let mut template = shell();
    template["entities"]["food_safety__limit"] = json!({"fields": {"x": {"type": "string"}}});
    let p = try_parse(TWO).unwrap();
    assert_eq!(
        compose(&template, "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::Clash("food_safety__limit".into())
    );
}

#[test]
fn a_plugin_with_no_entities_composes_as_before() {
    let p = try_parse("").unwrap();
    assert!(p.entities.is_empty());
    let c = compose(&shell(), "business.inventory", Some((&p, DIGEST))).unwrap();
    assert!(!c.manifest["entities"]
        .as_object()
        .unwrap()
        .keys()
        .any(|k| k.contains("__")));
}
