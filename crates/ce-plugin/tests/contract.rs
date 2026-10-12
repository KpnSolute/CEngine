use ce_plugin::{parse, Module, Registry, Slot};
use semver::Version;

fn registry() -> Registry {
    Registry::new().with(Module {
        id: "business.inventory".into(),
        api: 1,
        slots: vec![
            Slot {
                name: "valuation".into(),
                rule: Some(true),
                params: vec!["places".into()],
            },
            Slot {
                name: "price_review".into(),
                rule: Some(false),
                params: vec!["threshold".into()],
            },
        ],
        read: vec!["inventory.items".into(), "inventory.periods".into()],
        write: vec!["inventory.periods".into()],
        emit: vec!["inventory.review_requested".into()],
    })
}

const ENGINE: Version = Version::new(0, 2, 0);

const GOOD: &str = r#"
schema_version = 1

[plugin]
id = "inventory-rules"
version = "0.1.0"
description = "test"

[compatibility]
engine = ">=0.1.0, <2.0.0"
module = "business.inventory"
module_api = 1

[execution]
kind = "policy-pack"

[capabilities]
read = ["inventory.items"]
write = ["inventory.periods"]
emit = ["inventory.review_requested"]

[[policy]]
slot = "valuation"
rule = "round(qty * price, 2)"
params = { places = 2 }

[[policy]]
slot = "price_review"
params = { threshold = "0.25" }

[tests]
fixtures = "fixtures"
"#;

fn codes(text: &str) -> Vec<&'static str> {
    match parse(text, &registry(), &ENGINE) {
        Ok(_) => vec![],
        Err(v) => v.iter().map(|x| x.code).collect(),
    }
}

fn with(from: &str, to: &str) -> String {
    assert!(GOOD.contains(from), "fixture missing {from}");
    GOOD.replacen(from, to, 1)
}

#[test]
fn accepts_a_valid_pack() {
    let p = parse(GOOD, &registry(), &ENGINE).unwrap();
    assert_eq!(p.id, "inventory-rules");
    assert_eq!(p.policies.len(), 2);
    assert_eq!(p.policies[1].params["threshold"], "0.25");
    assert_eq!(p.fixtures.as_deref(), Some("fixtures"));
}

#[test]
fn rejects_unknown_fields_and_malformed_toml() {
    assert_eq!(
        codes(&with("[execution]", "surprise = 1\n[execution]")),
        vec!["malformed"]
    );
    assert_eq!(codes("not = [valid"), vec!["malformed"]);
}

#[test]
fn rejects_unsupported_hosts_and_network() {
    assert_eq!(
        codes(&with("policy-pack", "isolated-python")),
        vec!["unsupported_host"]
    );
    assert_eq!(
        codes(&with(
            "[capabilities]",
            "[capabilities]\nexternal_network = true"
        )),
        vec!["network_denied"]
    );
}

#[test]
fn rejects_bad_identity_and_versions() {
    assert_eq!(
        codes(&with("inventory-rules", "Inventory_Rules")),
        vec!["bad_id"]
    );
    assert_eq!(codes(&with("\"0.1.0\"", "\"one\"")), vec!["bad_version"]);
    assert_eq!(
        codes(&with(">=0.1.0, <2.0.0", ">=3.0.0")),
        vec!["engine_incompatible"]
    );
    assert_eq!(
        codes(&with(">=0.1.0, <2.0.0", "lots")),
        vec!["bad_engine_range"]
    );
    assert_eq!(
        codes(&with("schema_version = 1", "schema_version = 2")),
        vec!["unsupported_schema"]
    );
}

#[test]
fn rejects_module_mismatches() {
    assert_eq!(
        codes(&with("business.inventory", "business.payroll")),
        vec!["unknown_module"]
    );
    assert_eq!(
        codes(&with("module_api = 1", "module_api = 2")),
        vec!["module_api_mismatch"]
    );
}

#[test]
fn rejects_excess_and_duplicate_capabilities() {
    assert_eq!(
        codes(&with(
            "read = [\"inventory.items\"]",
            "read = [\"payroll.runs\"]"
        )),
        vec!["capability_denied"]
    );
    assert_eq!(
        codes(&with(
            "read = [\"inventory.items\"]",
            "read = [\"inventory.items\", \"inventory.items\"]"
        )),
        vec!["duplicate"]
    );
}

#[test]
fn rejects_bad_policies() {
    assert_eq!(
        codes(&with("slot = \"valuation\"", "slot = \"mystery\"")),
        vec!["unknown_slot"]
    );
    assert_eq!(
        codes(&with("rule = \"round(qty * price, 2)\"\n", "")),
        vec!["rule_required"]
    );
    assert_eq!(
        codes(&with(
            "slot = \"price_review\"",
            "slot = \"price_review\"\nrule = \"1\""
        )),
        vec!["rule_forbidden"]
    );
    assert_eq!(
        codes(&with("round(qty * price, 2)", "round(qty *")),
        vec!["bad_rule"]
    );
    assert_eq!(
        codes(&with("params = { places = 2 }", "params = { digits = 2 }")),
        vec!["unknown_param"]
    );
    assert_eq!(
        codes(&with(
            "params = { places = 2 }",
            "params = { places = 2.5 }"
        )),
        vec!["bad_param"]
    );
    assert_eq!(
        codes(&with(
            "slot = \"price_review\"",
            "slot = \"valuation\"\nrule = \"1\""
        )),
        vec!["duplicate_slot", "unknown_param"]
    );
}

#[test]
fn rejects_path_escapes() {
    for bad in [
        "../secrets",
        "/etc/passwd",
        "a/../b",
        "C:/x",
        "a\\\\b",
        "a//b",
        "./x",
        "",
    ] {
        assert_eq!(
            codes(&with(
                "fixtures = \"fixtures\"",
                &format!("fixtures = \"{bad}\"")
            )),
            vec!["bad_path"],
            "{bad}"
        );
    }
}

#[test]
fn rejects_oversize_descriptors() {
    let big = format!("{GOOD}\n# {}", "x".repeat(70_000));
    assert_eq!(codes(&big), vec!["too_large"]);
}
