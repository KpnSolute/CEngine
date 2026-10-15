//! Plugin event subscriptions: parsed, limited to the plugin's own entities, and composed into
//! the manifest with the namespaced entity name.

use ce_plugin::{builtin_registry, compose, parse, ComposeError};
use semver::Version;
use serde_json::{json, Value};

const ENGINE: Version = Version::new(0, 1, 0);
const DIGEST: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

const HEAD: &str = r#"schema_version = 1
[plugin]
id = "archiver"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"

[[entity]]
name = "job"
[entity.definition.fields.changeset]
type = "string"
required = true
"#;

const SUB: &str = r#"
[[subscribe]]
event = "changeset.merged"
entity = "job"
[subscribe.values.changeset]
from = "subject"
"#;

fn shell(with_events: bool) -> Value {
    let mut m = json!({
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
    });
    if with_events {
        m["entities"]["outbox_event"] = json!({
            "fields": {
                "name": {"type": "string", "required": true, "max_length": 120},
                "data": {"type": "json"},
                "at": {"type": "integer", "required": true},
                "seq": {"type": "integer", "required": true}
            }
        });
    }
    m
}

fn codes(extra: &str) -> Vec<&'static str> {
    parse(&format!("{HEAD}{extra}"), &builtin_registry(), &ENGINE)
        .unwrap_err()
        .into_iter()
        .map(|v| v.code)
        .collect()
}

#[test]
fn a_subscription_composes_with_the_namespaced_entity() {
    let p = parse(&format!("{HEAD}{SUB}"), &builtin_registry(), &ENGINE).unwrap();
    assert_eq!(p.subscriptions.len(), 1);
    let c = compose(&shell(true), "business.inventory", Some((&p, DIGEST))).unwrap();
    let subs = c.manifest["subscriptions"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["event"], "changeset.merged");
    assert_eq!(subs[0]["create"]["entity"], "archiver__job");
    assert_eq!(subs[0]["create"]["values"]["changeset"]["from"], "subject");
}

#[test]
fn reactions_need_the_events_tool() {
    let p = parse(&format!("{HEAD}{SUB}"), &builtin_registry(), &ENGINE).unwrap();
    assert_eq!(
        compose(&shell(false), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::MissingEntity("outbox_event".into())
    );
}

#[test]
fn a_plugin_can_only_react_into_its_own_entities() {
    let t = "\n[[subscribe]]\nevent = \"changeset.merged\"\nentity = \"stock_item\"\n[subscribe.values.sku]\nvalue = \"X\"\n";
    assert!(codes(t).contains(&"unknown_plugin_entity"));
}

#[test]
fn bad_event_names_and_empty_values_are_refused() {
    let t = "\n[[subscribe]]\nevent = \"Changeset\"\nentity = \"job\"\n[subscribe.values.changeset]\nfrom = \"subject\"\n";
    assert!(codes(t).contains(&"bad_event_name"));
    let t = "\n[[subscribe]]\nevent = \"changeset.merged\"\nentity = \"job\"\n[subscribe.values]\n";
    assert!(codes(t).contains(&"no_values"));
}

#[test]
fn too_many_subscriptions_are_refused() {
    let one = "\n[[subscribe]]\nevent = \"changeset.merged\"\nentity = \"job\"\n[subscribe.values.changeset]\nfrom = \"subject\"\n";
    assert!(codes(&one.repeat(17)).contains(&"too_many"));
}

#[test]
fn a_bad_source_or_unknown_field_is_refused_at_compose_time() {
    for body in [
        "[subscribe.values.changeset]\nfrom = \"secrets\"\n",
        "[subscribe.values.nope]\nvalue = \"x\"\n",
        "[subscribe.values.changeset]\nfrom = \"subject\"\nvalue = \"x\"\n",
    ] {
        let t = format!("\n[[subscribe]]\nevent = \"changeset.merged\"\nentity = \"job\"\n{body}");
        let p = parse(&format!("{HEAD}{t}"), &builtin_registry(), &ENGINE).unwrap();
        assert!(
            matches!(
                compose(&shell(true), "business.inventory", Some((&p, DIGEST))),
                Err(ComposeError::Invalid(_))
            ),
            "{body}"
        );
    }
}
