use ce_manifest::validate;
use serde_json::{json, Value};

fn example(name: &str) -> Value {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn codes(v: &Value) -> Vec<&'static str> {
    match validate(&serde_json::to_vec(v).unwrap()) {
        Ok(_) => vec![],
        Err(i) => i.into_iter().map(|x| x.code).collect(),
    }
}

#[test]
fn the_three_example_templates_are_valid() {
    for n in ["inventory", "booking_crm", "approval_workflow"] {
        let r = validate(&serde_json::to_vec(&example(n)).unwrap());
        assert!(r.is_ok(), "{n}: {:?}", r.err());
    }
}

#[test]
fn hash_is_stable_and_changes_with_content() {
    let a = validate(&serde_json::to_vec(&example("booking_crm")).unwrap()).unwrap();
    let b = validate(&serde_json::to_vec(&example("booking_crm")).unwrap()).unwrap();
    assert_eq!(a.hash, b.hash);
    let mut m = example("booking_crm");
    m["template"]["version"] = json!("0.1.1");
    let c = validate(&serde_json::to_vec(&m).unwrap()).unwrap();
    assert_ne!(a.hash, c.hash);
}

#[test]
fn schema_errors_are_reported() {
    let mut m = example("booking_crm");
    m["manifest_version"] = json!(2);
    assert!(codes(&m).contains(&"schema"));
    assert_eq!(validate(b"not json").unwrap_err()[0].code, "json");
}

#[test]
fn semantic_errors_are_caught() {
    let mut m = example("booking_crm");
    m["entities"]["booking"]["fields"]["contact"]["ref"] = json!("nobody");
    assert!(codes(&m).contains(&"unknown_entity"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["state"]["states"]["requested"]["to"] = json!(["ghost"]);
    assert!(codes(&m).contains(&"unknown_state"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["state"]["states"]["cancelled"]["code"] = json!(200);
    assert!(codes(&m).contains(&"duplicate_state_code"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["invariants"][0]["expr"] = json!("ends_at > nope");
    assert!(codes(&m).contains(&"unknown_identifier"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["invariants"][0]["expr"] = json!("ends_at >");
    assert!(codes(&m).contains(&"expr_parse"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["invariants"][0]["expr"] = json!("frobnicate(ends_at)");
    assert!(codes(&m).contains(&"unknown_function"));

    let mut m = example("booking_crm");
    m["entities"]["booking"]["sort_keys"] = json!(["notes"]);
    assert!(codes(&m).contains(&"unindexed_sort_key"));

    let mut m = example("booking_crm");
    m["actions"]["confirm_booking"]["effect"][0]["transition"] = json!("flying");
    assert!(codes(&m).contains(&"unknown_state"));

    let mut m = example("booking_crm");
    m["projections"]["public_slots"]["fields"] = json!(["starts_at", "notes", "ghost"]);
    assert!(codes(&m).contains(&"unknown_field"));

    let mut m = example("booking_crm");
    m["entities"]["contact"]["fields"]["email"]["pii"] = json!(true);
    m["projections"]["public_slots"]["entity"] = json!("contact");
    m["projections"]["public_slots"]["fields"] = json!(["email"]);
    m["projections"]["public_slots"]["filter"] = json!("full_name != null");
    assert!(codes(&m).contains(&"pii_in_public_projection"));

    let mut m = example("booking_crm");
    m["roles"]["owner"]["actions"] = json!(["missing"]);
    assert!(codes(&m).contains(&"unknown_action"));
}

#[test]
fn state_field_must_match_declared_enum() {
    let mut m = example("booking_crm");
    m["entities"]["booking"]["fields"]["status"]["values"] = json!(["a", "b"]);
    assert!(codes(&m).contains(&"state_field_mismatch"));
}

#[test]
fn aggregates_only_in_computed_and_filter_rules() {
    let mut m = example("inventory");
    m["entities"]["item"]["invariants"] =
        json!([{"name": "x", "expr": "count(children(item, period)) > 0"}]);
    assert!(codes(&m).contains(&"forbidden_function"));

    let mut m = example("inventory");
    m["projections"] = json!({"p": {"entity": "item", "fields": ["name"], "filter": "ref(period).state == 'open'"}});
    assert!(codes(&m).contains(&"forbidden_function"));
}

#[test]
fn computed_cycles_are_rejected() {
    let mut m = example("inventory");
    m["entities"]["item"]["fields"]["a"] =
        json!({"type": "decimal", "precision": 10, "scale": 2, "computed": "b + 1"});
    m["entities"]["item"]["fields"]["b"] =
        json!({"type": "decimal", "precision": 10, "scale": 2, "computed": "a + 1"});
    assert!(codes(&m).contains(&"computed_cycle"));
}

#[test]
fn children_sums_are_checked_against_the_entities() {
    let sum = |expr: &str| {
        let mut m = example("inventory");
        m["entities"]["period"]["fields"]["total"] =
            json!({"type": "decimal", "precision": 16, "scale": 2, "computed": expr});
        codes(&m)
    };
    assert!(sum("sum(children(item, period).value)").is_empty());
    assert!(sum("count(children(item, period))").is_empty());
    assert!(sum("sum(children(nope, period).value)").contains(&"unknown_entity"));
    assert!(sum("sum(children(item, name).value)").contains(&"bad_children"));
    assert!(sum("sum(children(item, period).ghost)").contains(&"unknown_field"));
    assert!(sum("sum(children(item))").contains(&"bad_children"));
    assert!(sum("sum(children(item, period).value.ghost)").contains(&"bad_children"));
}
