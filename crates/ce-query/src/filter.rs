use crate::plan::{Ctx, Param, QueryError};
use serde_json::Value;

const MAX_FILTER_DEPTH: usize = 8;
const MAX_FILTER_NODES: usize = 64;
const MAX_IN_LIST: usize = 100;

/// Compile a JSON filter tree to a SQL boolean expression.
///
/// Forms: `{"and":[..]}`, `{"or":[..]}`, `{"not":{..}}`, `{"field":"f","op":"eq","value":v}`.
pub fn compile(
    ctx: &mut Ctx,
    f: &Value,
    depth: usize,
    nodes: &mut usize,
) -> Result<String, QueryError> {
    *nodes += 1;
    if depth > MAX_FILTER_DEPTH || *nodes > MAX_FILTER_NODES {
        return Err(QueryError::FilterTooLarge);
    }
    let o = f
        .as_object()
        .ok_or(QueryError::BadFilter("filter node must be an object"))?;
    if let Some(list) = o.get("and").or_else(|| o.get("or")) {
        let word = if o.contains_key("and") { "AND" } else { "OR" };
        let items = list
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or(QueryError::BadFilter("and/or needs a non-empty list"))?;
        let parts: Result<Vec<_>, _> = items
            .iter()
            .map(|i| compile(ctx, i, depth + 1, nodes))
            .collect();
        return Ok(format!("({})", parts?.join(&format!(" {word} "))));
    }
    if let Some(inner) = o.get("not") {
        return Ok(format!("(NOT {})", compile(ctx, inner, depth + 1, nodes)?));
    }
    let field = o
        .get("field")
        .and_then(Value::as_str)
        .ok_or(QueryError::BadFilter("missing field"))?;
    let op = o
        .get("op")
        .and_then(Value::as_str)
        .ok_or(QueryError::BadFilter("missing op"))?;
    let col = ctx.column(field)?;
    let ty = ctx.field_type(field)?;
    if matches!(ty.as_str(), "json" | "file") {
        return Err(QueryError::NotFilterable(field.to_string()));
    }
    let val = o.get("value");
    match op {
        "null" => {
            let want = val.and_then(Value::as_bool).unwrap_or(true);
            Ok(format!("({col} IS {}NULL)", if want { "" } else { "NOT " }))
        }
        "eq" | "ne" | "gt" | "gte" | "lt" | "lte" => {
            let sym = match op {
                "eq" => "=",
                "ne" => "<>",
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                _ => "<=",
            };
            if matches!(ty.as_str(), "boolean" | "enum" | "ref") && !matches!(op, "eq" | "ne") {
                return Err(QueryError::BadOp(op.to_string(), field.to_string()));
            }
            let (p, cast) = ctx.value(field, val.ok_or(QueryError::BadFilter("missing value"))?)?;
            let n = ctx.bind(p);
            Ok(format!("({col} {sym} ${n}{cast})"))
        }
        "in" | "nin" => {
            let items = val
                .and_then(Value::as_array)
                .filter(|a| !a.is_empty() && a.len() <= MAX_IN_LIST)
                .ok_or(QueryError::BadFilter("in needs a list of 1 to 100 values"))?;
            let mut marks = Vec::new();
            for it in items {
                let (p, cast) = ctx.value(field, it)?;
                let n = ctx.bind(p);
                marks.push(format!("${n}{cast}"));
            }
            let not = if op == "nin" { "NOT " } else { "" };
            Ok(format!("({col} {not}IN ({}))", marks.join(", ")))
        }
        "like" | "ilike" => {
            if !matches!(ty.as_str(), "string" | "text") {
                return Err(QueryError::BadOp(op.to_string(), field.to_string()));
            }
            let s = val
                .and_then(Value::as_str)
                .ok_or(QueryError::BadFilter("like needs a string"))?;
            let n = ctx.bind(Param::Text(s.to_string()));
            Ok(format!(
                "({col} {} ${n})",
                if op == "like" { "LIKE" } else { "ILIKE" }
            ))
        }
        other => Err(QueryError::BadOp(other.to_string(), field.to_string())),
    }
}
