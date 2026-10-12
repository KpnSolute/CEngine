//! Computed fields: `fields.<name>.computed` expressions evaluated against the pending record,
//! in dependency order, before invariants run and before the record is stored.
//!
//! Callers can never set a computed field (validation refuses it); the server fills it here.
//! Decimals are rounded half-up to the field's declared scale and must fit its precision;
//! integers must come out whole. A null result removes the field. Sums over other records
//! (`sum`, `count`, `children`) and `ref()` need a [`Resolver`]; without one such a field is left
//! alone (absent on create), and invariants that read it are skipped.

use crate::invariants::{build_env, Context};
use crate::FieldError;
use bigdecimal::{BigDecimal, RoundingMode};
use ce_expr::Value as Ev;
use ce_expr::{Expr, Resolver};
use ce_manifest::Manifest;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// Calls and names an expression uses, from the parsed tree (not the source text).
pub(crate) struct Uses {
    pub calls: Vec<String>,
    pub names: Vec<String>,
    /// `(entity, ref_field)` of every `children(entity, ref_field)`.
    pub children: Vec<(String, String)>,
}

fn walk(e: &Expr, u: &mut Uses) {
    match e {
        Expr::List(xs) => xs.iter().for_each(|x| walk(x, u)),
        Expr::Path(p) => {
            // `old.x` and `actor.x` are bindings, not fields of the pending record.
            if let Some(first) = p.first() {
                if p.len() == 1 || !matches!(first.as_str(), "old" | "actor") {
                    u.names.push(first.clone());
                }
            }
        }
        Expr::Call(n, xs) => {
            u.calls.push(n.clone());
            if let (true, [Expr::Path(e), Expr::Path(f)]) = (n == "children", xs.as_slice()) {
                if let ([e], [f]) = (e.as_slice(), f.as_slice()) {
                    u.children.push((e.clone(), f.clone()));
                }
            }
            xs.iter().for_each(|x| walk(x, u));
        }
        Expr::Field(x, _) | Expr::Neg(x) | Expr::Not(x) => walk(x, u),
        Expr::Bin(_, a, b) => {
            walk(a, u);
            walk(b, u);
        }
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Dec(_) | Expr::Str(_) => {}
    }
}

/// None when the source does not parse (the manifest checker rejects those earlier).
pub(crate) fn uses(src: &str) -> Option<Uses> {
    let e = ce_expr::parse(src).ok()?;
    let mut u = Uses {
        calls: Vec::new(),
        names: Vec::new(),
        children: Vec::new(),
    };
    walk(&e, &mut u);
    Some(u)
}

/// True when the expression needs other records, which this evaluator does not have.
pub(crate) fn needs_other_records(u: &Uses) -> bool {
    u.calls
        .iter()
        .any(|c| matches!(c.as_str(), "sum" | "count" | "children" | "ref"))
}

/// Computed fields that cannot be evaluated: those that need other records, and, through the
/// dependency graph, those that read such a field.
pub(crate) fn unsupported_fields(ent: &Value) -> HashSet<String> {
    let computed: Vec<(String, Option<Uses>)> = ent["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, f)| f["computed"].as_str().map(|e| (k.clone(), uses(e))))
        .collect();
    let mut out: HashSet<String> = computed
        .iter()
        .filter(|(_, u)| u.as_ref().is_none_or(needs_other_records))
        .map(|(k, _)| k.clone())
        .collect();
    loop {
        let before = out.len();
        for (k, u) in &computed {
            if u.as_ref()
                .is_some_and(|u| u.names.iter().any(|n| out.contains(n)))
            {
                out.insert(k.clone());
            }
        }
        if out.len() == before {
            return out;
        }
    }
}

/// A parent entity whose computed fields sum or count the records of `child` that point at it
/// through `ref_field`. Writing such a child record changes the parent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AggregateSource {
    pub parent: String,
    pub child: String,
    pub ref_field: String,
}

/// Every `children(child, ref_field)` that a computed field of the manifest reads, sorted.
pub fn aggregate_sources(m: &Manifest) -> Vec<AggregateSource> {
    let mut out = Vec::new();
    for (parent, ent) in m.value["entities"].as_object().into_iter().flatten() {
        for f in ent["fields"]
            .as_object()
            .into_iter()
            .flat_map(|f| f.values())
        {
            if let Some(u) = f["computed"].as_str().and_then(uses) {
                for (child, ref_field) in u.children {
                    out.push(AggregateSource {
                        parent: parent.clone(),
                        child,
                        ref_field,
                    });
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn fail(field: &str, detail: &str) -> FieldError {
    FieldError {
        path: format!("/{field}"),
        code: "computed_failed",
        detail: detail.to_string(),
    }
}

fn to_json(name: &str, f: &Value, v: Ev) -> Result<Option<Value>, String> {
    let ty = f["type"].as_str().unwrap_or("");
    let out = match (ty, v) {
        (_, Ev::Null) => {
            if f["required"] == true {
                return Err("a required field cannot be empty".to_string());
            }
            None
        }
        ("integer", Ev::Int(i)) => Some(Value::from(i)),
        ("integer", _) => return Err("the result is not a whole number".to_string()),
        ("decimal", v) => {
            let d: BigDecimal = match v {
                Ev::Int(i) => BigDecimal::from(i),
                Ev::Dec(d) => d,
                _ => return Err("the result is not a number".to_string()),
            };
            let scale = f["scale"].as_i64().unwrap_or(0);
            let precision = f["precision"].as_u64().unwrap_or(38);
            let d = d.with_scale_round(scale, RoundingMode::HalfUp);
            let whole = (d.digits() as i64 - scale).max(0);
            if whole > precision as i64 - scale {
                return Err("the result does not fit the declared precision".to_string());
            }
            Some(Value::String(d.to_string()))
        }
        ("boolean", Ev::Bool(b)) => Some(Value::Bool(b)),
        ("string" | "text" | "enum" | "date" | "datetime", Ev::Str(s)) => Some(Value::String(s)),
        _ => return Err("the result has the wrong type for this field".to_string()),
    };
    // The result must satisfy the whole field definition, like a caller's value would.
    if let Some(v) = &out {
        if let Some(e) = crate::check_value(name, f, v) {
            return Err(e.detail);
        }
    }
    Ok(out)
}

/// Evaluates every computed field of `entity` into `record`. Returns one error per field whose
/// expression failed or gave a value that does not fit the field.
///
/// Without a resolver, fields that need other records are left alone (see the module docs).
pub fn evaluate_computed(
    m: &Manifest,
    entity: &str,
    record: &mut Map<String, Value>,
    old: Option<&Map<String, Value>>,
    ctx: &Context,
) -> Result<(), Vec<FieldError>> {
    evaluate_computed_with(m, entity, record, old, ctx, None)
}

/// Like [`evaluate_computed`], and with a resolver also evaluates `ref()` and sums and counts
/// over `children(...)`, so no computed field is skipped.
pub fn evaluate_computed_with(
    m: &Manifest,
    entity: &str,
    record: &mut Map<String, Value>,
    old: Option<&Map<String, Value>>,
    ctx: &Context,
    resolver: Option<&dyn Resolver>,
) -> Result<(), Vec<FieldError>> {
    let ent = &m.value["entities"][entity];
    let Some(fields) = ent["fields"].as_object() else {
        return Ok(());
    };
    let skip = if resolver.is_some() {
        HashSet::new()
    } else {
        unsupported_fields(ent)
    };
    let mut todo: Vec<(&String, &str)> = fields
        .iter()
        .filter_map(|(k, f)| f["computed"].as_str().map(|e| (k, e)))
        .filter(|(k, _)| !skip.contains(*k))
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
            let ran = match resolver {
                Some(r) => ce_expr::run_with(src, &env, r),
                None => ce_expr::run(src, &env),
            };
            let out = match ran {
                Ok(v) => to_json(name, f, v).map_err(|d| fail(name, &d)),
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
