//! Python bindings: `import cengine`. No logic lives here; each function calls `ce-bind` and
//! returns its JSON text, so `json.loads(cengine.eval_expr(...))` gives the same answer as the
//! Rust, WebAssembly and server paths.

use pyo3::prelude::*;

/// Checks a manifest. Returns JSON: `{"ok": true, "hash": ...}` or `{"ok": false, "issues": [...]}`.
#[pyfunction]
fn validate_manifest(manifest_json: &str) -> String {
    ce_bind::validate_manifest(manifest_json)
}

/// Evaluates an expression. `env_json` is a JSON object; decimals are written `{"dec": "1.50"}`.
#[pyfunction]
fn eval_expr(expr: &str, env_json: &str) -> String {
    ce_bind::eval_expr(expr, env_json)
}

/// Checks a record body against a manifest entity; `mode` is "create" or "patch".
#[pyfunction]
fn validate_record(manifest_json: &str, entity: &str, record_json: &str, mode: &str) -> String {
    ce_bind::validate_record(manifest_json, entity, record_json, mode)
}

#[pymodule]
fn cengine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(validate_manifest, m)?)?;
    m.add_function(wrap_pyfunction!(eval_expr, m)?)?;
    m.add_function(wrap_pyfunction!(validate_record, m)?)?;
    Ok(())
}
