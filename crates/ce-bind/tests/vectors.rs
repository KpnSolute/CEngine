//! The shared expression vectors, run through the binding core's JSON interface.
use serde_json::Value;

fn manifest() -> String {
    std::fs::read_to_string(format!(
        "{}/../../spec/examples/inventory.json",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn expression_vectors_through_json() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../conformance/expression-vectors.json"
    );
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut bad = Vec::new();
    for t in doc["vectors"].as_array().unwrap() {
        let got: Value = serde_json::from_str(&ce_bind::eval_expr(
            t["expr"].as_str().unwrap(),
            &t["env"].to_string(),
        ))
        .unwrap();
        let want = match &t["out"] {
            Value::Bool(b) => Value::String(b.to_string()),
            other => other.clone(),
        };
        if got["ok"] != true || got["value"] != want {
            bad.push(format!("{}: want {want}, got {got}", t["expr"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn bad_input_is_a_value_not_a_panic() {
    for (e, env) in [
        ("1 +", "{}"),
        ("x", "not json"),
        ("x", "[]"),
        ("x", r#"{"x": 1.5}"#),
        ("x", r#"{"x": {"dec": "abc"}}"#),
    ] {
        let r: Value = serde_json::from_str(&ce_bind::eval_expr(e, env)).unwrap();
        assert_eq!(r["ok"], false, "{e} {env}");
    }
}

#[test]
fn manifests_and_records() {
    let ok: Value = serde_json::from_str(&ce_bind::validate_manifest(&manifest())).unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["hash"].as_str().unwrap().len(), 64);
    let bad: Value = serde_json::from_str(&ce_bind::validate_manifest("{}")).unwrap();
    assert_eq!(bad["ok"], false);
    assert!(!bad["issues"].as_array().unwrap().is_empty());

    let good = ce_bind::validate_record(
        &manifest(),
        "item",
        r#"{"name":"Flour","unit_cost":"2.5"}"#,
        "create",
    );
    assert_eq!(
        serde_json::from_str::<Value>(&good).unwrap()["ok"],
        true,
        "{good}"
    );
    let r: Value = serde_json::from_str(&ce_bind::validate_record(
        &manifest(),
        "item",
        r#"{"name":"Flour","value":"1"}"#,
        "create",
    ))
    .unwrap();
    assert_eq!(r["ok"], false);
    let codes: Vec<&str> = r["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    assert!(
        codes.contains(&"computed_field") && codes.contains(&"required"),
        "{codes:?}"
    );
    let bad_mode: Value =
        serde_json::from_str(&ce_bind::validate_record(&manifest(), "item", "{}", "x")).unwrap();
    assert_eq!(bad_mode["ok"], false);
}
