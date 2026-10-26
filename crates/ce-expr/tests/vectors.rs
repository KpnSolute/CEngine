//! Runs conformance/expression-vectors.json against the Rust evaluator.
use bigdecimal::BigDecimal;
use ce_expr::{run, Env, Value};
use std::str::FromStr;

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => Value::Int(n.as_i64().expect("int")),
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Object(o) => Value::Dec(
            BigDecimal::from_str(o["dec"].as_str().expect("dec string")).expect("decimal"),
        ),
        serde_json::Value::Array(a) => Value::List(a.iter().map(json_to_value).collect()),
    }
}

#[test]
fn conformance_vectors() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../conformance/expression-vectors.json"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let vectors = doc["vectors"].as_array().unwrap();
    let mut failures = Vec::new();
    for t in vectors {
        let env: Env = t["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), json_to_value(v)))
            .collect();
        let expr = t["expr"].as_str().unwrap();
        let got = run(expr, &env).map(|v| v.canonical());
        let want = t["out"]
            .as_str()
            .map(String::from)
            .or_else(|| t["out"].as_bool().map(|b| b.to_string()));
        if got != Ok(want.clone()) {
            failures.push(format!("{expr}: want {want:?}, got {got:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} failed:\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

#[test]
fn limits_are_enforced() {
    use ce_expr::{parse, ParseError, MAX_DEPTH, MAX_LEN};
    assert_eq!(parse(&"1+".repeat(MAX_LEN)), Err(ParseError::TooLong));
    let deep = format!(
        "{}1{}",
        "(".repeat(MAX_DEPTH + 2),
        ")".repeat(MAX_DEPTH + 2)
    );
    assert_eq!(parse(&deep), Err(ParseError::TooDeep));
}

#[test]
fn errors_not_panics() {
    let env = Env::new();
    assert!(run("1 +", &env).is_err());
    assert!(run("'a' + 1", &env).is_err());
    assert!(run("nope(1)", &env).is_err());
    assert!(run("9223372036854775807 + 1", &env).is_err());
    assert_eq!(run("5 % 0", &env).map(|v| v.canonical()), Ok(None));
}

#[test]
fn date_parts_validate_the_whole_date_and_time() {
    let env = Env::new();
    let ok = |e: &str| run(e, &env).map(|v| v.canonical());
    assert_eq!(ok("day('2028-02-29')"), Ok(Some("29".into())));
    assert_eq!(ok("year('2026-11-01T08:00:00Z')"), Ok(Some("2026".into())));
    assert_eq!(ok("month('2026-10-26 08:00')"), Ok(Some("10".into())));
    assert!(run("day('2026-02-31')", &env).is_err());
    assert!(run("day('2027-02-29')", &env).is_err());
    assert!(run("month('2026-13-01')", &env).is_err());
    assert!(run("year('2026-10-26Tgarbage')", &env).is_err());
    assert!(run("year('2026-10-26T25:00:00Z')", &env).is_err());
}

#[test]
fn trim_floor_and_ceil() {
    use ce_expr::{run, Env, Value};
    let env = Env::new();
    assert_eq!(run("trim('  a b ')", &env), Ok(Value::Str("a b".into())));
    assert_eq!(run("trim(null)", &env), Ok(Value::Null));
    assert_eq!(run("floor(2)", &env), Ok(Value::Int(2)));
    assert_eq!(run("floor(2.5)", &env).unwrap().canonical().unwrap(), "2");
    assert_eq!(run("floor(-2.5)", &env).unwrap().canonical().unwrap(), "-3");
    assert_eq!(run("ceil(2.1)", &env).unwrap().canonical().unwrap(), "3");
    assert_eq!(run("ceil(-2.1)", &env).unwrap().canonical().unwrap(), "-2");
}
