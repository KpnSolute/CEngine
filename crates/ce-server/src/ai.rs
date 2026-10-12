//! AI-assisted data entry (`POST .../_ai/entry`): free text in, checked draft records out.
//!
//! CEngine never calls a model. It asks the host's [`Provider`] (Infajn) for an `ai` connector
//! `records.extract` call, sending only the entity's shape and the caller's text, never tenant
//! data or credentials. Whatever comes back is untrusted: each draft is cut down to declared,
//! settable fields and run through the same validation as any create. Nothing is stored. The
//! caller reviews the drafts and saves them through the ordinary write, batch or import routes,
//! so rules, policy and the audit trail apply exactly as for typed entry.

use ce_manifest::Manifest;
use ce_provider::{ConnectorType, ProviderError, ProviderRequest};
use ce_validate::{validate, Mode, RESERVED};
use serde_json::{json, Map, Value};

/// The most draft records one request returns.
pub(crate) const MAX_DRAFTS: usize = 50;
/// The most bytes of text one request may carry.
pub(crate) const MAX_TEXT: usize = 20_000;
pub(crate) const OPERATION: &str = "records.extract";

/// The fields a draft may set: declared, not computed, not the state field.
fn settable<'a>(m: &'a Manifest, entity: &str) -> Vec<(&'a String, &'a Value)> {
    let ent = &m.value["entities"][entity];
    let state = ent["state"]["field"].as_str();
    ent["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| {
            !v["computed"].is_string()
                && Some(k.as_str()) != state
                && !RESERVED.contains(&k.as_str())
        })
        .collect()
}

/// What the provider is told about the entity: field names and types, nothing about any tenant.
pub(crate) fn request(
    m: &Manifest,
    entity: &str,
    text: &str,
    hint: Option<&str>,
) -> ProviderRequest {
    let fields: Map<String, Value> = settable(m, entity)
        .into_iter()
        .map(|(k, v)| {
            let mut f = Map::new();
            for key in [
                "type",
                "required",
                "max_length",
                "values",
                "ref",
                "min",
                "max",
            ] {
                if !v[key].is_null() {
                    f.insert(key.to_string(), v[key].clone());
                }
            }
            (k.clone(), Value::Object(f))
        })
        .collect();
    ProviderRequest::read(
        ConnectorType::Ai,
        OPERATION,
        json!({"entity": entity, "fields": fields, "text": text, "hint": hint, "max_records": MAX_DRAFTS}),
    )
}

/// One draft with the problems validation found (empty when it is ready to save).
pub(crate) struct Draft {
    pub body: Map<String, Value>,
    pub errors: Vec<Value>,
}

/// Turns the provider's answer (`{"records": [...]}`) into checked drafts. Anything that is not
/// an object is skipped; unknown or unsettable keys are dropped and reported on the draft.
pub(crate) fn drafts(
    m: &Manifest,
    entity: &str,
    data: &Value,
) -> Result<Vec<Draft>, ProviderError> {
    let Some(records) = data["records"].as_array() else {
        return Err(ProviderError::Failed(
            "the AI answer had no records list".into(),
        ));
    };
    let allowed: Vec<&String> = settable(m, entity).into_iter().map(|(k, _)| k).collect();
    let mut out = Vec::new();
    for r in records.iter().filter_map(Value::as_object).take(MAX_DRAFTS) {
        let mut body = Map::new();
        let mut errors = Vec::new();
        for (k, v) in r {
            if allowed.contains(&k) {
                if !v.is_null() {
                    body.insert(k.clone(), v.clone());
                }
            } else {
                errors.push(json!({"path": format!("/{k}"), "code": "unknown_field",
                    "detail": "the AI proposed a field this entity cannot take; it was dropped"}));
            }
        }
        if let Err(es) = validate(m, entity, &body, Mode::Create) {
            errors.extend(
                es.into_iter()
                    .map(|e| json!({"path": e.path, "code": e.code, "detail": e.detail})),
            );
        }
        out.push(Draft { body, errors });
    }
    Ok(out)
}
