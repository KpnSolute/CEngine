//! Computed fields: `fields.<name>.computed` expressions evaluated against the pending record,
//! in dependency order, before invariants run and before the record is stored.
//!
//! Callers can never set a computed field (validation refuses it); the server fills it here.
//! Decimals are rounded half-up to the field's declared scale and must fit its precision;
//! integers must come out whole. A null result removes the field. Sums over other records
//! (`sum`, `count`, `children`) and `ref()` are not evaluable yet: such a field is left alone
//! (absent on create), and invariants that read it are skipped.

use crate::invariants::{build_env, Context};
use crate::FieldError;
use bigdecimal::{BigDecimal, RoundingMode};
use ce_expr::Value as Ev;
use ce_manifest::Manifest;
use serde_json::{Map, Value};

/// True when the expression needs other records, which this evaluator does not have.
pub(crate) fn uncomputable(expr: &str) -> bool {
    ["sum", "count", "children", "ref"]
        .iter()
        .any(|f| expr.contains(&format!("{f}(")))
}

fn fail(field: &str, detail: &str) -> FieldError {
    FieldError {
        path: format!("/{field}"),
        code: "computed_failed",
        detail: detail.to_string(),
    }
}

fn to_json(f: &Value, v: Ev) -> Result<Option<Value>, &'static str> {
    let ty = f["type"].as_str().unwrap_or("");
    Ok(match (ty, v) {
        (_, Ev::Null) => None,
        ("integer", Ev::Int(i)) => Some(Value::from(i)),
        ("integer", _) => return Err("the result is not a whole number"),
        ("decimal", v) => {
            let d: BigDecimal = match v {
                Ev::Int(i) => BigDecimal::from(i),
                Ev::Dec(d) => d,
                _ => return Err("the result is not a number"),
            };
            let scale = f["scale"].as_i64().unwrap_or(0);
            let precision = f["precision"].as_u64().unwrap_or(38);
            let d = d.with_scale_round(scale, RoundingMode::HalfUp);
            let whole = (d.digits() as i64 - scale).max(0);
            if whole > precision as i64 - scale {
                return Err("the result does not fit the declared precision");
            }
            Some(Value::String(d.to_string()))
        }
        ("boolean", Ev::Bool(b)) => Some(Value::Bool(b)),
        ("string" | "text" | "enum" | "date" | "datetime", Ev::Str(s)) => Some(Value::String(s)),
        _ => return Err("the result has the wrong type for this field"),
    })
}

/// Evaluates every computed field of `entity` into `record`. Returns one error per field whose
/// expression failed or gave a value that does not fit the field.
pub fn evaluate_computed(
    m: &Manifest,
    entity: &str,
    record: &mut Map<String, Value>,
    old: Option<&Map<String, Value>>,
    ctx: &Context,
) -> Result<(), Vec<FieldError>> {
    let ent = &m.value["entities"][entity];
    let Some(fields) = ent["fields"].as_object() else {
        return Ok(());
    };
    let mut todo: Vec<(&String, &str)> = fields
        .iter()
        .filter_map(|(k, f)| f["computed"].as_str().map(|e| (k, e)))
        .filter(|(_, e)| !uncomputable(e))
        .collect();
    todo.sort();
    // The manifest checker rejects cycles, so n passes settle every chain of n fields.
    let mut errors: Vec<FieldError> = Vec::new();
    for _ in 0..=todo.len() {
        errors.clear();
        let mut changed = false;
        for (name, src) in &todo {
            let env = build_env(ent, record, old, ctx);
            let f = &fields[name.as_str()];
            let out = match ce_expr::run(src, &env) {
                Ok(v) => to_json(f, v).map_err(|d| fail(name, d)),
                Err(_) => Err(fail(name, "the expression could not be evaluated")),
            };
            match out {
                Ok(Some(v)) => {
                    if record.get(name.as_str()) != Some(&v) {
                        record.insert((*name).clone(), v);
                        changed = true;
                    }
                }
                Ok(None) => {
                    if record.remove(name.as_str()).is_some() {
                        changed = true;
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        if !changed {
            break;
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
