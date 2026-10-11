//! Invariants: the manifest's `invariants[].expr` checked against a whole record with the
//! expression evaluator. A false or null result is a failure (spec/expression-language.md).
//!
//! Values map to expression types by field type: strings, booleans and refs as they are,
//! integers as `int`, decimals as `decimal`, dates as strings (`YYYY-MM-DD` sorts correctly),
//! datetimes as UTC strings with fixed nanoseconds, so comparing two datetimes in different
//! zones is right (nanosecond precision). Missing fields are null; on create the manifest
//! default fills them first, on patch it does not (a removed field is null).
//!
//! Bindings besides fields: `old.field` (null on create), `actor.id`, `actor.role`, `now`,
//! `today`. Computed fields are available once `evaluate_computed` has run. `ref(...)` and
//! computed sums over other records cannot be resolved here yet, so an invariant that reads
//! one is skipped rather than failing every write; `unenforced_invariants` lists them.

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
                        .to_rfc3339_opts(SecondsFormat::Nanos, true),
                )
            }),
            _ => Ev::Str(s.clone()),
        },
        _ => Ev::Null,
    }
}

/// Request context for the `actor.*`, `now` and `today` bindings.
#[derive(Debug, Clone)]
pub struct Context {
    pub actor_id: String,
    pub actor_role: Option<String>,
    pub now_unix: i64,
}

/// An invariant that is declared but not enforced yet, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unenforced {
    pub entity: String,
    pub name: String,
    pub reason: &'static str,
}

fn unenforceable(ent: &Value, src: &str) -> Option<&'static str> {
    let Some(u) = crate::computed::uses(src) else {
        return Some("does not parse");
    };
    if u.calls.iter().any(|c| c == "ref") {
        return Some("reads another record with ref()");
    }
    if crate::computed::needs_other_records(&u) {
        return Some("sums other records");
    }
    let skip = crate::computed::unsupported_fields(ent);
    if u.names.iter().any(|n| skip.contains(n)) {
        return Some("reads a computed field that is a sum over other records");
    }
    None
}

/// Invariants the checker skips today (they read a computed field or use `ref`).
pub fn unenforced_invariants(m: &Manifest) -> Vec<Unenforced> {
    let mut out = Vec::new();
    let Some(ents) = m.value["entities"].as_object() else {
        return out;
    };
    for (en, ent) in ents {
        for inv in ent["invariants"].as_array().into_iter().flatten() {
            if let Some(reason) = inv["expr"].as_str().and_then(|e| unenforceable(ent, e)) {
                out.push(Unenforced {
                    entity: en.clone(),
                    name: inv["name"].as_str().unwrap_or("invariant").to_string(),
                    reason,
                });
            }
        }
    }
    out
}

pub(crate) fn build_env(
    ent: &Value,
    record: &Map<String, Value>,
    old: Option<&Map<String, Value>>,
    ctx: &Context,
) -> Env {
    let mut env = Env::new();
    if let Some(fields) = ent["fields"].as_object() {
        for (name, f) in fields {
            let ty = f["type"].as_str().unwrap_or("");
            let current = match (record.get(name), old) {
                (Some(v), _) => v,
                (None, None) => f.get("default").unwrap_or(&Value::Null),
                (None, Some(_)) => &Value::Null,
            };
            env.insert(name.clone(), to_expr(ty, current));
            let before = old.and_then(|o| o.get(name)).unwrap_or(&Value::Null);
            env.insert(format!("old.{name}"), to_expr(ty, before));
        }
    }
    env.insert("actor.id".into(), Ev::Str(ctx.actor_id.clone()));
    env.insert(
        "actor.role".into(),
        ctx.actor_role.clone().map_or(Ev::Null, Ev::Str),
    );
    if let Some(t) = DateTime::from_timestamp(ctx.now_unix, 0) {
        env.insert(
            "now".into(),
            Ev::Str(t.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        );
        env.insert("today".into(), Ev::Str(t.format("%Y-%m-%d").to_string()));
    }
    env
}

/// Checks every enforceable invariant of `entity` against `record` (the whole record as it
/// would be stored). `old` is the stored record on patch and `None` on create. Returns one
/// error per failed invariant, path `""`, code `invariant_failed`, detail the invariant's
/// `message_code` (or its name).
pub fn check_invariants(
    m: &Manifest,
    entity: &str,
    record: &Map<String, Value>,
    old: Option<&Map<String, Value>>,
    ctx: &Context,
) -> Result<(), Vec<FieldError>> {
    let ent = &m.value["entities"][entity];
    let Some(list) = ent["invariants"].as_array() else {
        return Ok(());
    };
    let env = build_env(ent, record, old, ctx);
    let mut errors = Vec::new();
    for inv in list {
        let Some(src) = inv["expr"].as_str() else {
            continue;
        };
        if unenforceable(ent, src).is_some() {
            continue;
        }
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
