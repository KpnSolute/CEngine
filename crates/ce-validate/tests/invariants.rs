use ce_manifest::validate as validate_manifest;
use ce_validate::check_invariants;
use serde_json::{json, Map, Value};

fn manifest() -> ce_manifest::Manifest {
    let m = json!({
        "manifest_version": 1,
        "template": {"name": "t", "version": "0.1.0", "title": "T"},
        "entities": {"booking": {
            "fields": {
                "starts_at": {"type": "datetime", "required": true},
                "ends_at": {"type": "datetime", "required": true},
                "seats": {"type": "integer", "default": 1},
                "price": {"type": "decimal", "precision": 8, "scale": 2}
            },
            "invariants": [
                {"name": "window", "expr": "ends_at > starts_at", "message_code": "booking.invalid_window"},
                {"name": "seats", "expr": "seats >= 1"},
                {"name": "price", "expr": "price >= 0"}
            ]
        }},
        "roles": {"admin": {"read": ["*"], "write": ["*"]}},
        "runtime": {"storage": {"relational": true}}
    });
    validate_manifest(m.to_string().as_bytes()).expect("test manifest is valid")
}

fn run(v: Value) -> Result<(), Vec<String>> {
    let rec: Map<String, Value> = v.as_object().unwrap().clone();
    check_invariants(&manifest(), "booking", &rec)
        .map_err(|e| e.into_iter().map(|x| x.detail).collect())
}

#[test]
fn a_record_that_satisfies_every_rule_passes() {
    // `price` is absent (null), which is a failure for `price >= 0`, so give it.
    let ok = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "price": "0"});
    assert_eq!(run(ok), Ok(()));
}

#[test]
fn a_false_rule_reports_its_message_code_or_name() {
    let rec = json!({"starts_at": "2026-01-01T11:00:00Z", "ends_at": "2026-01-01T10:00:00Z", "price": "5.00"});
    assert_eq!(run(rec), Err(vec!["booking.invalid_window".to_string()]));
}

#[test]
fn a_null_result_counts_as_failure() {
    let rec = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z"});
    assert_eq!(run(rec), Err(vec!["price".to_string()]));
}

#[test]
fn datetimes_in_different_zones_compare_by_instant() {
    // 12:00+02:00 is 10:00Z, which is before 10:30Z even though "12:00" sorts after "10:30".
    let rec = json!({"starts_at": "2026-01-01T12:00:00+02:00", "ends_at": "2026-01-01T10:30:00Z", "price": 1});
    assert_eq!(run(rec), Ok(()));
}

#[test]
fn defaults_fill_missing_fields() {
    let rec = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "price": "1.5"});
    assert_eq!(run(rec), Ok(())); // seats defaults to 1
}

#[test]
fn every_failed_rule_is_reported() {
    let rec = json!({"starts_at": "2026-01-01T11:00:00Z", "ends_at": "2026-01-01T10:00:00Z", "seats": 0, "price": "-1"});
    assert_eq!(
        run(rec),
        Err(vec![
            "booking.invalid_window".into(),
            "seats".into(),
            "price".into()
        ])
    );
}
