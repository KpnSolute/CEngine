//! WebAssembly bindings for JavaScript (browser, Node, Deno, Workers). No logic lives here; each
//! function calls `ce-bind` and returns its JSON text, so `JSON.parse(evalExpr(...))` gives the
//! same answer as the Rust, Python and server paths. The same validator runs in the form layer,
//! so the browser and the server reject the same records.

use wasm_bindgen::prelude::*;

/// Checks a manifest. Returns JSON: `{"ok":true,"hash":...}` or `{"ok":false,"issues":[...]}`.
#[wasm_bindgen(js_name = validateManifest)]
pub fn validate_manifest(manifest_json: &str) -> String {
    ce_bind::validate_manifest(manifest_json)
}

/// Evaluates an expression. `env_json` is a JSON object; decimals are written `{"dec":"1.50"}`.
#[wasm_bindgen(js_name = evalExpr)]
pub fn eval_expr(expr: &str, env_json: &str) -> String {
    ce_bind::eval_expr(expr, env_json)
}

/// Checks a record body against a manifest entity; `mode` is "create" or "patch".
#[wasm_bindgen(js_name = validateRecord)]
pub fn validate_record(manifest_json: &str, entity: &str, record_json: &str, mode: &str) -> String {
    ce_bind::validate_record(manifest_json, entity, record_json, mode)
}
