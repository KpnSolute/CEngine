//! Plugin deliveries: parsed, limited to the plugin's own entities and non-money connectors, and
//! composed into the manifest with the namespaced entity name.

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
[entity.definition.fields.status]
type = "string"
[entity.definition.fields.attempts]
type = "integer"
[entity.definition.fields.not_before]
type = "integer"
[entity.definition.fields.last_error]
type = "string"
"#;

const DELIVER: &str = r#"
[[deliver]]
entity = "job"
connector = "storage"
operation = "archive.put"
[deliver.params.changeset]
from = "changeset"
[deliver.params.kind]
value = "snapshot"
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
fn a_delivery_composes_with_the_namespaced_entity() {
    let p = parse(&format!("{HEAD}{DELIVER}"), &builtin_registry(), &ENGINE).unwrap();
    assert_eq!(p.deliveries.len(), 1);
    let c = compose(&shell(true), "business.inventory", Some((&p, DIGEST))).unwrap();
    let d = &c.manifest["deliveries"][0];
    assert_eq!(d["entity"], "archiver__job");
    assert_eq!(d["connector"], "storage");
    assert_eq!(d["operation"], "archive.put");
    assert_eq!(d["params"]["changeset"]["from"], "changeset");
    assert_eq!(d["params"]["kind"]["value"], "snapshot");
}

#[test]
fn deliveries_need_the_events_tool() {
    let p = parse(&format!("{HEAD}{DELIVER}"), &builtin_registry(), &ENGINE).unwrap();
    assert_eq!(
        compose(&shell(false), "business.inventory", Some((&p, DIGEST))).unwrap_err(),
        ComposeError::MissingEntity("outbox_event".into())
    );
}

#[test]
fn a_delivery_may_only_queue_the_plugins_own_entities() {
    let t = DELIVER.replace("entity = \"job\"", "entity = \"stock_item\"");
    assert!(codes(&t).contains(&"unknown_plugin_entity"));
}

#[test]
fn money_moving_and_unknown_connectors_are_refused() {
    for c in ["banking", "payments", "hr_payroll"] {
        let t = DELIVER.replace("storage", c);
        assert!(codes(&t).contains(&"connector_not_allowed"), "{c}");
    }
    let t = DELIVER.replace("storage", "carrier_pigeon");
    assert!(codes(&t).contains(&"unknown_connector"));
}

#[test]
fn the_entity_needs_the_queue_fields_and_params_must_name_its_fields() {
    let plain = HEAD
        .split("[entity.definition.fields.status]")
        .next()
        .unwrap();
    let got: Vec<&str> = parse(&format!("{plain}{DELIVER}"), &builtin_registry(), &ENGINE)
        .unwrap_err()
        .into_iter()
        .map(|v| v.code)
        .collect();
    assert_eq!(
        got.iter().filter(|c| **c == "missing_queue_field").count(),
        4
    );
    let t = DELIVER.replace("from = \"changeset\"", "from = \"nope\"");
    assert!(codes(&t).contains(&"bad_param"));
    let t = DELIVER.replace("archive.put", "Archive");
    assert!(codes(&t).contains(&"bad_operation"));
}

/// A plugin whose job entity breaks the relay's rules is refused when it is composed.
fn compose_codes(plugin: &str) -> Vec<String> {
    let p = parse(plugin, &builtin_registry(), &ENGINE).unwrap();
    match compose(&shell(true), "business.inventory", Some((&p, DIGEST))).unwrap_err() {
        ComposeError::Invalid(v) => v.into_iter().map(|(code, _, _)| code).collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn queue_fields_the_relay_cannot_write_safely_are_refused() {
    let base = format!("{HEAD}{DELIVER}");
    // a state machine that owns `status`
    let t =
        format!("{base}\n[entity.definition.state]\nfield = \"status\"\ninitial = \"queued\"\n[entity.definition.state.states.queued]\ncode = 100\n");
    // (the state model needs its own transitions; only the ownership matters here)
    let c = compose_codes(&t);
    assert!(
        c.contains(&"state_managed_queue_field".to_string()),
        "{c:?}"
    );
    // a required field the relay clears
    let t = base.replace(
        "[entity.definition.fields.last_error]\ntype = \"string\"",
        "[entity.definition.fields.last_error]\ntype = \"string\"\nrequired = true",
    );
    assert!(compose_codes(&t).contains(&"required_queue_field".to_string()));
    // a result that is not json
    let t = format!("{base}\n[entity.definition.fields.result]\ntype = \"integer\"\n");
    assert!(compose_codes(&t).contains(&"result_not_json".to_string()));
    // an enum status that lacks a relay value
    let t = base.replace(
        "[entity.definition.fields.status]\ntype = \"string\"",
        "[entity.definition.fields.status]\ntype = \"enum\"\nvalues = [\"queued\", \"done\"]",
    );
    assert!(compose_codes(&t).contains(&"missing_status_value".to_string()));
}
