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
            "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true}
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

/// True when any of the three products behind a line's money is an exact half-cent, the one
/// place exact decimal rounding and the legacy float rounding can disagree.
fn ties(i: &Value) -> bool {
    let price = dec(i["unit_price"].as_str().unwrap());
    let ouc = dec(i["opening_unit_cost"].as_str().unwrap());
    let open = dec(i["opening_oh"].as_str().unwrap());
    let open_cost = if ouc > 0 { ouc } else { price.clone() };
    [
        open * open_cost,
        sum(&i["received"]) * price.clone(),
        sum(&i["pulled"]) * price,
    ]
    .into_iter()
    .any(|p| {
        let p = p * BigDecimal::from(200);
        p.is_integer() && p.with_scale(0).digits() > 0 && (&p % BigDecimal::from(2)) != 0
    })
}

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
    let mut tie_diffs: Vec<String> = Vec::new();
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
        // Money: to the cent. The only accepted difference is the legacy float rounding of an
        // exact half-cent product (1.005 is stored below 1.005, so Python rounds it down):
        // the module rounds the exact product half up. See `ties` below.
        let tie = ties(i);
        for name_of in [
            "opening_value",
            "received_value",
            "pulled_value",
            "ending_value",
        ] {
            let got = dec(rec[name_of].as_str().unwrap());
            let exp = dec(&cents(want[name_of].as_str().unwrap()));
            let diff = (&got - &exp).abs();
            if diff == 0 {
                continue;
            }
            if tie && diff <= dec("0.03") {
                tie_diffs.push(format!("{name}: {name_of} {got} vs legacy {exp}"));
            } else {
                bad.push(format!(
                    "{name}: {name_of} {got} != legacy {exp} (input {})",
                    c["input"]
                ));
            }
        }
    }
    eprintln!(
        "half-cent ties where exact half-up differs from legacy float rounding: {}\n{}",
        tie_diffs.len(),
        tie_diffs.join("\n")
    );
    assert!(
        bad.is_empty(),
        "{} of {} vectors differ:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}
