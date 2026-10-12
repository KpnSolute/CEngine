//! CEngine template manifest validation (draft v1). See `spec/manifest.schema.json`.
//!
//! `validate` runs the JSON Schema, then semantic checks the schema cannot express
//! (references, state graphs, expressions). It returns every issue it finds.

mod projection;
mod semantic;

pub use projection::filter_json;

use serde_json::Value;
use sha2::{Digest, Sha256};

const SCHEMA: &str = include_str!("../../../spec/manifest.schema.json");

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    /// JSON Pointer to the offending location.
    pub path: String,
    /// Stable machine code, e.g. `schema`, `unknown_entity`.
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub value: Value,
    /// Hex SHA-256 of the compact, key-sorted JSON.
    pub hash: String,
}

pub(crate) fn issue(
    path: impl Into<String>,
    code: &'static str,
    message: impl Into<String>,
) -> Issue {
    Issue {
        path: path.into(),
        code,
        message: message.into(),
    }
}

pub fn validate(json: &[u8]) -> Result<Manifest, Vec<Issue>> {
    let value: Value =
        serde_json::from_slice(json).map_err(|e| vec![issue("", "json", e.to_string())])?;
    let schema: Value = serde_json::from_str(SCHEMA).expect("embedded schema is valid JSON");
    let validator = jsonschema::validator_for(&schema).expect("embedded schema compiles");
    let mut issues: Vec<Issue> = validator
        .iter_errors(&value)
        .map(|e| issue(e.instance_path.to_string(), "schema", e.to_string()))
        .collect();
    if issues.is_empty() {
        issues.extend(semantic::check(&value));
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    // serde_json maps are key-sorted (no preserve_order), so this is deterministic.
    let canonical = serde_json::to_vec(&value).expect("serialises");
    let hash = Sha256::digest(&canonical)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Manifest { value, hash })
}
