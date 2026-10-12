//! Parity between the legacy MJCC inventory formulas and the `business.inventory` module under
//! the MJCC valuation policy pack.
//!
//! `tests/parity/mjcc_inventory_vectors.json` holds synthetic rows and the figures the legacy
//! Python (`backend/inventory_formulas.py::resolve_row_financials`) produced for them; the
//! generator is next to it. Every quantity must match exactly (the legacy sums floats, so a
//! rounding hair of 1e-9 is allowed there) and every money figure must match to the cent.
use bigdecimal::BigDecimal;
use ce_plugin::{builtin_registry, compose, parse};
use ce_validate::{evaluate_computed, Context};
use semver::Version;
use serde_json::{json, Map, Value};
use std::str::FromStr;

const ENGINE: Version = Version::new(0, 2, 0);
const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn dec(s: &str) -> BigDecimal {
    BigDecimal::from_str(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

fn shell() -> Value {
    json!({
        "manifest_version": 1,
        "template": {"name": "mjcc_parity", "version": "0.1.0"},
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

/// The composed manifest with the two movement sums turned into plain inputs, so a line can be
/// evaluated without a store.
fn manifest() -> ce_manifest::Manifest {
    let text = include_str!("fixtures/mjcc-legacy-valuation/plugin.toml");
    let plugin = parse(text, &builtin_registry(), &ENGINE).unwrap_or_else(|e| panic!("{e:?}"));
    let composed = compose(&shell(), "business.inventory", Some((&plugin, DIGEST)))
        .unwrap_or_else(|e| panic!("{e:?}"));
    let mut m = composed.manifest;
    for f in ["received", "pulled"] {
        m["entities"]["stock_line"]["fields"][f]
            .as_object_mut()
            .unwrap()
            .remove("computed");
    }
    ce_manifest::validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"))
}

fn sum(v: &Value) -> BigDecimal {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| dec(x.as_str().unwrap()))
        .fold(BigDecimal::from(0), |a, b| a + b)
}

/// (case, field, CEngine, legacy): exact half-cent products where exact half-up differs from the
/// legacy float rounding. Anything else that differs is a failure.
const KNOWN_FLOAT_ROUNDING: [(&str, &str, &str, &str); 4] = [
    ("tie product 1.005 x 1", "opening_value", "1.01", "1.00"),
    ("tie product 1.005 x 1", "ending_value", "1.01", "1.00"),
    (
        "tie product 0.045 x 1 on received",
        "received_value",
        "0.05",
        "0.04",
    ),
    (
        "tie product 0.045 x 1 on received",
        "ending_value",
        "0.05",
        "0.04",
    ),
];

fn cents(legacy: &str) -> String {
    format!("{:.2}", legacy.parse::<f64>().unwrap())
}

#[test]
fn every_legacy_vector_matches_the_module_under_the_mjcc_policy() {
    let m = manifest();
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../tests/parity/mjcc_inventory_vectors.json"
    ))
    .unwrap();
    let ctx = Context {
        actor_id: "u1".into(),
        actor_role: Some("owner".into()),
        now_unix: 1_700_000_000,
    };
    let cases = vectors["cases"].as_array().unwrap();
    assert!(cases.len() >= 100, "too few vectors: {}", cases.len());
    let mut bad: Vec<String> = Vec::new();
    let mut seen = [false; KNOWN_FLOAT_ROUNDING.len()];
    for c in cases {
        let i = &c["input"];
        let mut rec: Map<String, Value> = json!({
            "item": "i", "period": "p",
            "opening_oh": i["opening_oh"],
            "unit_price": i["unit_price"],
            "opening_unit_cost": i["opening_unit_cost"],
            "received": sum(&i["received"]).to_string(),
            "pulled": sum(&i["pulled"]).to_string(),
        })
        .as_object()
        .unwrap()
        .clone();
        let name = c["name"].as_str().unwrap();
        if let Err(e) = evaluate_computed(&m, "stock_line", &mut rec, None, &ctx) {
            bad.push(format!("{name}: evaluation failed {e:?}"));
            continue;
        }
        let want = &c["legacy"];
        // Quantity, within a rounding hair of the legacy float sum.
        let end_q = dec(rec["ending_qty"].as_str().unwrap());
        let want_q = dec(&format!(
            "{:.9}",
            want["ending_qty"].as_str().unwrap().parse::<f64>().unwrap()
        ));
        if (&end_q - &want_q).abs() > dec("0.000000001") {
            bad.push(format!("{name}: ending_qty {end_q} != {want_q}"));
        }
        // Money: to the cent. The only accepted differences are the four figures in
        // `KNOWN_FLOAT_ROUNDING`: the legacy float read path rounds an exact half cent one cent
        // down (1.005 is stored below 1.005), the module rounds the exact product half up.
        for name_of in [
            "opening_value",
            "received_value",
            "pulled_value",
            "ending_value",
        ] {
            let got = rec[name_of].as_str().unwrap().to_string();
            let exp = cents(want[name_of].as_str().unwrap());
            if dec(&got) == dec(&exp) {
                continue;
            }
            let known = KNOWN_FLOAT_ROUNDING
                .iter()
                .position(|k| *k == (name, name_of, got.as_str(), exp.as_str()));
            match known {
                Some(at) => seen[at] = true,
                None => bad.push(format!(
                    "{name}: {name_of} {got} != legacy {exp} (input {})",
                    c["input"]
                )),
            }
        }
    }
    for (k, hit) in KNOWN_FLOAT_ROUNDING.iter().zip(&seen) {
        if !hit {
            bad.push(format!(
                "expected float-rounding difference not seen: {k:?}"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} vectors differ:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}

/// Prices where the legacy float arithmetic holds a move of exactly 25% (for example
/// 57.946660 to 43.459995, which is exactly three quarters). The legacy tests and rule text say
/// exactly 25% applies, and the exact decimal rule does that. Anything else that differs from
/// the legacy decision is a failure.
const KNOWN_FLOAT_HOLDS: [(&str, &str); 6] = [
    ("57.946660", "43.459995"),
    ("82.172024", "102.715030"),
    ("70.860152", "88.575190"),
    ("8.808492", "11.010615"),
    ("2.100712", "1.575534"),
    ("32.815008", "41.018760"),
];

#[test]
fn price_decisions_match_the_legacy_observed_price_protocol() {
    let m = manifest();
    let rule = m.value["actions"]["settle_price_observation"]["effect"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|e| e["set"]["decision"].as_str())
        .expect("the decision rule is a string")
        .to_string();
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../tests/parity/mjcc_price_vectors.json"
    ))
    .unwrap();
    let cases = vectors["cases"].as_array().unwrap();
    assert!(cases.len() >= 300, "too few vectors: {}", cases.len());
    let mut bad: Vec<String> = Vec::new();
    let mut seen = [false; KNOWN_FLOAT_HOLDS.len()];
    for c in cases {
        let (prev, obs) = (
            c["previous"].as_str().unwrap(),
            c["observed"].as_str().unwrap(),
        );
        let mut env = ce_expr::Env::new();
        env.insert("previous_price".into(), ce_expr::Value::Dec(dec(prev)));
        env.insert("observed_price".into(), ce_expr::Value::Dec(dec(obs)));
        let got = match ce_expr::run(&rule, &env) {
            Ok(ce_expr::Value::Str(s)) => s,
            other => panic!("{prev} -> {obs}: {other:?}"),
        };
        let want = c["decision"].as_str().unwrap();
        if got == want {
            continue;
        }
        match KNOWN_FLOAT_HOLDS.iter().position(|k| *k == (prev, obs)) {
            Some(at) if want == "hold" && got == "apply" => seen[at] = true,
            _ => bad.push(format!("{prev} -> {obs}: {got}, legacy {want}")),
        }
    }
    for (k, hit) in KNOWN_FLOAT_HOLDS.iter().zip(&seen) {
        if !hit {
            bad.push(format!("expected float-hold difference not seen: {k:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
