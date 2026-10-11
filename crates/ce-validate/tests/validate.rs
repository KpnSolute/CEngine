use ce_manifest::{validate as validate_manifest, Manifest};
use ce_validate::*;
use serde_json::{json, Map, Value};

fn manifest() -> Manifest {
    let m = json!({
        "manifest_version": 1,
        "template": {"name": "t", "version": "0.1.0", "title": "T"},
        "entities": {
            "thing": {"fields": {
                "name":   {"type": "string", "required": true, "max_length": 5},
                "notes":  {"type": "text"},
                "count":  {"type": "integer"},
                "price":  {"type": "decimal", "precision": 6, "scale": 2},
                "active": {"type": "boolean"},
                "day":    {"type": "date"},
                "at":     {"type": "datetime"},
                "kind":   {"type": "enum", "values": ["a", "b"]},
                "owner":  {"type": "ref", "ref": "thing"},
                "extra":  {"type": "json"},
                "doc":    {"type": "file"},
                "total":  {"type": "decimal", "precision": 8, "scale": 2, "required": true,
                           "computed": "price * 2"}
            }}
        },
        "roles": {"admin": {"read": ["*"], "write": ["*"]}},
        "runtime": {"storage": {"relational": true}}
    });
    validate_manifest(m.to_string().as_bytes()).expect("test manifest is valid")
}

fn body(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn check(v: Value, mode: Mode) -> Result<(), Vec<(String, &'static str)>> {
    validate(&manifest(), "thing", &body(v), mode)
        .map_err(|e| e.into_iter().map(|x| (x.path, x.code)).collect())
}

fn one(v: Value) -> Vec<(String, &'static str)> {
    check(v, Mode::Create).unwrap_err()
}

fn bad(field: &str, value: Value, code: &'static str) {
    let mut b = json!({"name": "ok"});
    b[field] = value.clone();
    assert_eq!(one(b), [(format!("/{field}"), code)], "{field}={value}");
}

fn good(field: &str, value: Value) {
    let mut b = json!({"name": "ok"});
    b[field] = value.clone();
    assert_eq!(check(b, Mode::Create), Ok(()), "{field}={value}");
}

#[test]
fn a_minimal_valid_create_passes_and_computed_required_is_not_asked_for() {
    assert_eq!(check(json!({"name": "ok"}), Mode::Create), Ok(()));
}

#[test]
fn required_fields_and_nulls_on_create() {
    assert_eq!(one(json!({})), [("/name".to_string(), "required")]);
    assert_eq!(
        one(json!({"name": null})),
        [("/name".to_string(), "null_not_allowed")]
    );
    bad("count", json!(null), "null_not_allowed");
}

#[test]
fn engine_unknown_and_computed_fields_are_refused() {
    let got = one(json!({"name": "ok", "id": "x", "tenant_id": "t", "nope": 1, "total": 3}));
    assert_eq!(
        got,
        [
            ("/id".to_string(), "reserved_field"),
            ("/nope".to_string(), "unknown_field"),
            ("/tenant_id".to_string(), "reserved_field"),
            ("/total".to_string(), "computed_field"),
        ]
    );
}

#[test]
fn strings_respect_length_in_characters_and_refuse_nul() {
    good("name", json!("héllo")); // 5 characters, 6 bytes
    bad("name", json!("héllos"), "too_long");
    bad("name", json!(5), "type_mismatch");
    bad("notes", json!("a\u{0}b"), "invalid_value");
    good("notes", json!("x".repeat(10_000)));
}

#[test]
fn integers_are_whole_and_fit_64_bits() {
    good("count", json!(0));
    good("count", json!(-9_223_372_036_854_775_808_i64));
    bad("count", json!(1.5), "type_mismatch");
    bad("count", json!(1.0), "type_mismatch");
    bad("count", json!("7"), "type_mismatch");
    bad(
        "count",
        json!(9_223_372_036_854_775_808_u64),
        "out_of_range",
    );
}

#[test]
fn decimals_fit_precision_and_scale_without_rounding() {
    good("price", json!(1234.56));
    good("price", json!("1234.56"));
    good("price", json!(-0.5));
    good("price", json!(0));
    bad("price", json!(1.005), "out_of_range"); // 3 digits after the point
    bad("price", json!("12345.67"), "out_of_range"); // 5 digits before, only 4 allowed
    bad("price", json!(12345.6), "out_of_range");
    bad("price", json!("1e2"), "invalid_format");
    bad("price", json!(1e21), "invalid_format");
    bad("price", json!("1."), "invalid_format");
    bad("price", json!(".5"), "invalid_format");
    bad("price", json!("abc"), "invalid_format");
    bad("price", json!(true), "type_mismatch");
}

#[test]
fn booleans_dates_datetimes_enums_refs_json_and_files() {
    good("active", json!(false));
    bad("active", json!("true"), "type_mismatch");
    good("day", json!("2024-02-29"));
    bad("day", json!("2023-02-29"), "invalid_format");
    bad("day", json!("2024-2-9"), "invalid_format");
    bad("day", json!(20240229), "type_mismatch");
    good("at", json!("2026-10-10T12:00:00Z"));
    good("at", json!("2026-10-10T12:00:00.123+02:00"));
    bad("at", json!("2026-10-10T12:00:00"), "invalid_format");
    bad("at", json!("2026-10-10"), "invalid_format");
    good("kind", json!("a"));
    bad("kind", json!("c"), "invalid_value");
    bad("kind", json!(1), "type_mismatch");
    good("owner", json!("11111111-1111-1111-1111-111111111111"));
    bad("owner", json!("not-a-uuid"), "invalid_format");
    bad(
        "owner",
        json!("11111111111111111111111111111111"),
        "invalid_format",
    );
    good("extra", json!({"any": [1, 2, {"x": null}]}));
    good("extra", json!("anything"));
    good("doc", json!({"key": "abc"}));
    bad("doc", json!("abc"), "type_mismatch");
}

#[test]
fn patch_allows_omitting_required_and_removing_optional_but_not_removing_required() {
    assert_eq!(check(json!({}), Mode::Patch), Ok(()));
    assert_eq!(
        check(json!({"count": null, "kind": null}), Mode::Patch),
        Ok(())
    );
    assert_eq!(
        check(json!({"name": null}), Mode::Patch),
        Err(vec![("/name".to_string(), "required")])
    );
    // types are checked on patch too
    assert_eq!(
        check(json!({"price": "x"}), Mode::Patch),
        Err(vec![("/price".to_string(), "invalid_format")])
    );
    assert_eq!(
        check(json!({"id": "x"}), Mode::Patch),
        Err(vec![("/id".to_string(), "reserved_field")])
    );
}

#[test]
fn every_problem_is_reported_at_once() {
    let got = one(json!({"count": "x", "price": "y", "kind": "z"}));
    assert_eq!(
        got,
        [
            ("/count".to_string(), "type_mismatch"),
            ("/kind".to_string(), "invalid_value"),
            ("/price".to_string(), "invalid_format"),
            ("/name".to_string(), "required"),
        ]
    );
}

#[test]
fn unknown_entity_is_one_body_level_error() {
    let e = validate(&manifest(), "ghost", &Map::new(), Mode::Create).unwrap_err();
    assert_eq!(e.len(), 1);
    assert_eq!((e[0].path.as_str(), e[0].code), ("", "unknown_entity"));
}
