//! Invariants: the manifest's `invariants[].expr` checked against a whole record with the
//! expression evaluator. A false or null result is a failure (spec/expression-language.md).
//!
//! Values map to expression types by field type: strings, booleans and refs as they are,
//! integers as `int`, decimals as `decimal`, dates as strings (`YYYY-MM-DD` sorts correctly),
//! datetimes as UTC strings with fixed microseconds, so comparing two datetimes in different
//! zones is right. Missing fields are null; on create the manifest default fills them first.
//! Computed fields are not evaluated yet, so they read as null.

use crate::FieldError;
use bigdecimal::BigDecimal;
use ce_expr::{Env, Value as Ev};
use ce_manifest::Manifest;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value};
use std::str::FromStr;

fn to_expr(ty: &str, v: &Value) -> Ev {
    match v {
        Value::Null => Ev::Null,
        Value::Bool(b) => Ev::Bool(*b),
        Value::Number(n) => {
            if ty == "decimal" {
                BigDecimal::from_str(&n.to_string()).map_or(Ev::Null, Ev::Dec)
            } else {
                n.as_i64().map_or(Ev::Null, Ev::Int)
            }
        }
        Value::String(s) => match ty {
            "decimal" => BigDecimal::from_str(s).map_or(Ev::Null, Ev::Dec),
            "datetime" => DateTime::parse_from_rfc3339(s).map_or(Ev::Null, |d| {
                Ev::Str(
                    d.with_timezone(&Utc)
                        .to_rfc3339_opts(SecondsFormat::Micros, true),
                )
            }),
            _ => Ev::Str(s.clone()),
        },
        _ => Ev::Null,
    }
}

/// Checks every invariant of `entity` against `record` (the whole record as it would be
/// stored, or the create body). Returns one error per failed invariant, path `""`, code
/// `invariant_failed`, detail the invariant's `message_code` (or its name).
pub fn check_invariants(
    m: &Manifest,
    entity: &str,
    record: &Map<String, Value>,
) -> Result<(), Vec<FieldError>> {
    let ent = &m.value["entities"][entity];
    let Some(list) = ent["invariants"].as_array() else {
        return Ok(());
    };
    let mut env = Env::new();
    if let Some(fields) = ent["fields"].as_object() {
        for (name, f) in fields {
            let ty = f["type"].as_str().unwrap_or("");
            let v = record
                .get(name)
                .or_else(|| f.get("default"))
                .unwrap_or(&Value::Null);
            env.insert(name.clone(), to_expr(ty, v));
        }
    }
    let mut errors = Vec::new();
    for inv in list {
        let Some(src) = inv["expr"].as_str() else {
            continue;
        };
        let passed = matches!(ce_expr::run(src, &env), Ok(Ev::Bool(true)));
        if !passed {
            let what = inv["message_code"]
                .as_str()
                .or_else(|| inv["name"].as_str())
                .unwrap_or("invariant");
            errors.push(FieldError {
                path: String::new(),
                code: "invariant_failed",
                detail: what.to_string(),
            });
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
