use ce_manifest::validate as validate_manifest;
use ce_validate::{check_invariants, unenforced_invariants, Context};
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
                "price": {"type": "decimal", "precision": 8, "scale": 2},
                "note": {"type": "string", "default": "hi"},
                "owner": {"type": "ref", "ref": "booking"},
                "total": {"type": "decimal", "precision": 8, "scale": 2, "computed": "price * 2"}
            },
            "invariants": [
                {"name": "window", "expr": "ends_at > starts_at", "message_code": "booking.invalid_window"},
                {"name": "seats", "expr": "seats >= 1"},
                {"name": "price", "expr": "price >= 0"},
                {"name": "grows", "expr": "is_null(old.seats) or seats >= old.seats"},
                {"name": "double", "expr": "is_null(total) or total >= 0"},
                {"name": "owner", "expr": "ref(owner).state == 'open'"},
                {"name": "text", "expr": "trim(note) != '' and floor(2.5) == 2 and ceil(2.5) == 3"}
            ]
        }},
        "roles": {"admin": {"read": ["*"], "write": ["*"]}},
        "runtime": {"storage": {"relational": true}}
    });
    validate_manifest(m.to_string().as_bytes()).expect("test manifest is valid")
}

fn ctx() -> Context {
    Context {
        actor_id: "u1".into(),
        actor_role: Some("manager".into()),
        now_unix: 1_700_000_000,
    }
}

fn map(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn run(v: Value) -> Result<(), Vec<String>> {
    let mut rec = map(v);
    ce_validate::evaluate_computed(&manifest(), "booking", &mut rec, None, &ctx()).unwrap();
    check_invariants(&manifest(), "booking", &rec, None, &ctx())
        .map_err(|e| e.into_iter().map(|x| x.detail).collect())
}

fn patch(old: Value, new: Value) -> Result<(), Vec<String>> {
    let (old, mut new) = (map(old), map(new));
    ce_validate::evaluate_computed(&manifest(), "booking", &mut new, Some(&old), &ctx()).unwrap();
    check_invariants(&manifest(), "booking", &new, Some(&old), &ctx())
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
            "price".into(),
            "double".into()
        ])
    );
}

#[test]
fn a_patch_does_not_refill_a_removed_field_from_its_default() {
    let old = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "seats": 2, "price": "1"});
    // `seats` was removed by the patch: null, not the default 1, so `seats >= 1` fails.
    let new = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "price": "1"});
    assert!(patch(old, new).unwrap_err().contains(&"seats".to_string()));
}

#[test]
fn old_binding_sees_the_stored_record() {
    let old = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "seats": 3, "price": "1", "note": "x"});
    let lower = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "seats": 2, "price": "1", "note": "x"});
    assert_eq!(patch(old.clone(), lower), Err(vec!["grows".to_string()]));
    let higher = json!({"starts_at": "2026-01-01T10:00:00Z", "ends_at": "2026-01-01T11:00:00Z", "seats": 4, "price": "1", "note": "x"});
    assert_eq!(patch(old, higher), Ok(()));
}

#[test]
fn nanosecond_differences_are_kept() {
    let rec = json!({"starts_at": "2026-01-01T10:00:00.0000001Z", "ends_at": "2026-01-01T10:00:00.0000002Z", "price": 1});
    assert_eq!(run(rec), Ok(()));
}

#[test]
fn ref_invariants_are_skipped_and_listed() {
    let names: Vec<String> = unenforced_invariants(&manifest())
        .into_iter()
        .map(|u| u.name)
        .collect();
    assert_eq!(names, ["owner"]);
}

#[test]
fn computed_fields_are_evaluated_rounded_and_chained() {
    let m = validate_manifest(
        json!({
            "manifest_version": 1,
            "template": {"name": "t", "version": "0.1.0", "title": "T"},
            "entities": {"line": {"fields": {
                "qty": {"type": "decimal", "precision": 10, "scale": 3},
                "price": {"type": "decimal", "precision": 10, "scale": 2},
                "net": {"type": "decimal", "precision": 10, "scale": 2, "computed": "round(qty * price, 2)"},
                "gross": {"type": "decimal", "precision": 4, "scale": 2, "computed": "net * 2"},
                "label": {"type": "string", "computed": "concat('n=', 'x')"},
                "units": {"type": "integer", "computed": "5 % 3"}
            }}},
            "roles": {"admin": {"read": ["*"], "write": ["*"]}},
            "runtime": {"storage": {"relational": true}}
        })
        .to_string()
        .as_bytes(),
    )
    .expect("valid");
    let mut rec = map(json!({"qty": "0.125", "price": "0.20"}));
    let r = ce_validate::evaluate_computed(&m, "line", &mut rec, None, &ctx());
    // net = round(0.025, 2) = 0.03; gross = 0.06 fits numeric(4,2)
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(rec["net"], "0.03");
    assert_eq!(rec["gross"], "0.06");
    assert_eq!(rec["label"], "n=x");
    assert_eq!(rec["units"], 2);
}

#[test]
fn computed_results_that_do_not_fit_are_reported() {
    let m = validate_manifest(
        json!({
            "manifest_version": 1,
            "template": {"name": "t", "version": "0.1.0", "title": "T"},
            "entities": {"line": {"fields": {
                "a": {"type": "decimal", "precision": 10, "scale": 2},
                "big": {"type": "decimal", "precision": 4, "scale": 2, "computed": "a * 1000"}
            }}},
            "roles": {"admin": {"read": ["*"], "write": ["*"]}},
            "runtime": {"storage": {"relational": true}}
        })
        .to_string()
        .as_bytes(),
    )
    .expect("valid");
    let mut rec = map(json!({"a": "50.00"}));
    let e = ce_validate::evaluate_computed(&m, "line", &mut rec, None, &ctx()).unwrap_err();
    assert_eq!(e[0].path, "/big");
    assert_eq!(e[0].code, "computed_failed");
}
