//! The functions every CEngine binding exposes, as plain strings in and out.
//!
//! Bindings (`ce-py` for Python, `ce-wasm` for JavaScript) are thin wrappers over this crate, so
//! they cannot disagree: each function takes JSON text and returns JSON text, and never panics.
//! A failure is a value, `{"ok": false, ...}`, not an exception.

use bigdecimal::BigDecimal;
use ce_expr::{Env, Value};
use serde_json::{json, Map, Value as Json};
use std::str::FromStr;

/// Checks a manifest. `{"ok":true,"hash":"…"}` or `{"ok":false,"issues":[{path,code,message}]}`.
pub fn validate_manifest(manifest_json: &str) -> String {
    match ce_manifest::validate(manifest_json.as_bytes()) {
        Ok(m) => json!({"ok": true, "hash": m.hash}).to_string(),
        Err(issues) => {
            let issues: Vec<Json> = issues
                .into_iter()
                .map(|i| json!({"path": i.path, "code": i.code, "message": i.message}))
                .collect();
            json!({"ok": false, "issues": issues}).to_string()
        }
    }
}

/// Decodes the conformance env encoding: ints, bools, strings and null are plain JSON, decimals
/// are `{"dec": "1.50"}`, lists are arrays.
fn to_value(v: &Json) -> Result<Value, String> {
    Ok(match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Int(
            n.as_i64()
                .ok_or("numbers must be whole; write decimals as {\"dec\": \"1.50\"}")?,
        ),
        Json::String(s) => Value::Str(s.clone()),
        Json::Object(o) => {
            let d = o
                .get("dec")
                .and_then(Json::as_str)
                .ok_or("an object value must be {\"dec\": \"digits\"}")?;
            Value::Dec(BigDecimal::from_str(d).map_err(|_| "not a decimal".to_string())?)
        }
        Json::Array(a) => Value::List(a.iter().map(to_value).collect::<Result<_, _>>()?),
    })
}

/// Evaluates an expression of the expression language against a JSON environment.
/// `{"ok":true,"value":"3"}` (the canonical text; `null` for null) or `{"ok":false,"error":…}`.
pub fn eval_expr(expr: &str, env_json: &str) -> String {
    let fail = |e: String| json!({"ok": false, "error": e}).to_string();
    let env_doc: Json = match serde_json::from_str(env_json) {
        Ok(v) => v,
        Err(e) => return fail(format!("env is not JSON: {e}")),
    };
    let Some(obj) = env_doc.as_object() else {
        return fail("env must be a JSON object".into());
    };
    let mut env = Env::new();
    for (k, v) in obj {
        match to_value(v) {
            Ok(v) => {
                env.insert(k.clone(), v);
            }
            Err(e) => return fail(format!("env.{k}: {e}")),
        }
    }
    match ce_expr::run(expr, &env) {
        Ok(v) => json!({"ok": true, "value": v.canonical()}).to_string(),
        Err(e) => fail(format!("{e:?}")),
    }
}

/// Checks one record body against a manifest entity, the way the server checks a write.
/// `mode` is `"create"` or `"patch"`. `{"ok":true}` or `{"ok":false,"errors":[{path,code,detail}]}`.
pub fn validate_record(manifest_json: &str, entity: &str, record_json: &str, mode: &str) -> String {
    let fail = |e: String| json!({"ok": false, "error": e}).to_string();
    let m = match ce_manifest::validate(manifest_json.as_bytes()) {
        Ok(m) => m,
        Err(_) => return fail("the manifest is not valid".into()),
    };
    let mode = match mode {
        "create" => ce_validate::Mode::Create,
        "patch" => ce_validate::Mode::Patch,
        _ => return fail("mode must be create or patch".into()),
    };
    let body: Map<String, Json> = match serde_json::from_str(record_json) {
        Ok(Json::Object(o)) => o,
        _ => return fail("the record must be a JSON object".into()),
    };
    match ce_validate::validate(&m, entity, &body, mode) {
        Ok(()) => json!({"ok": true}).to_string(),
        Err(errors) => {
            let errors: Vec<Json> = errors
                .into_iter()
                .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail}))
                .collect();
            json!({"ok": false, "errors": errors}).to_string()
        }
    }
}
