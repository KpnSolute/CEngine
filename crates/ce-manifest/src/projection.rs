//! A projection's `filter` is an expression, but it runs as a query, so only the shapes the query
//! language can express are allowed: comparisons of one field with a literal, `in` with a list of
//! literals, `and`, `or` and `not`. [`filter_json`] turns one into the JSON filter the query
//! planner takes; the manifest checker reports any filter it cannot translate.

use ce_expr::{parse, Expr, Op};
use serde_json::{json, Value};

fn literal(e: &Expr) -> Option<Value> {
    match e {
        Expr::Bool(b) => Some(json!(b)),
        Expr::Int(i) => Some(json!(i)),
        Expr::Dec(d) => Some(json!(d.to_string())),
        Expr::Str(s) => Some(json!(s)),
        _ => None,
    }
}

fn field(e: &Expr) -> Option<&str> {
    match e {
        Expr::Path(p) if p.len() == 1 => Some(p[0].as_str()),
        _ => None,
    }
}

/// `negated` is true under an odd number of `not`s. A test against null is two-valued in SQL but
/// three-valued in the expression language (`not (x == null)` is never true), so it is refused
/// under `not`, and `x != null` is refused outright: neither could mean what it says here.
fn node(e: &Expr, negated: bool) -> Result<Value, String> {
    match e {
        Expr::Not(inner) => Ok(json!({"not": node(inner, !negated)?})),
        Expr::Bin(Op::And, a, b) => Ok(json!({"and": [node(a, negated)?, node(b, negated)?]})),
        Expr::Bin(Op::Or, a, b) => Ok(json!({"or": [node(a, negated)?, node(b, negated)?]})),
        Expr::Bin(Op::In, a, b) => {
            let f = field(a).ok_or("`in` needs a field on the left")?;
            let Expr::List(items) = &**b else {
                return Err("`in` needs a list on the right".into());
            };
            let values: Option<Vec<Value>> = items.iter().map(literal).collect();
            Ok(
                json!({"field": f, "op": "in", "value": values.ok_or("`in` lists hold literals only")?}),
            )
        }
        Expr::Bin(op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge), a, b) => {
            // `literal op field` reads as `field flipped-op literal`.
            let (f, lit, flip) = match (field(a), field(b)) {
                (Some(f), None) => (f, &**b, false),
                (None, Some(f)) => (f, &**a, true),
                _ => return Err("a comparison needs one field and one literal".into()),
            };
            let name = match (op, flip) {
                (Op::Eq, _) => "eq",
                (Op::Ne, _) => "ne",
                (Op::Lt, false) | (Op::Gt, true) => "lt",
                (Op::Le, false) | (Op::Ge, true) => "lte",
                (Op::Gt, false) | (Op::Lt, true) => "gt",
                _ => "gte",
            };
            if matches!(lit, Expr::Null) {
                return match op {
                    Op::Eq if !negated => Ok(json!({"field": f, "op": "null", "value": true})),
                    Op::Eq => Err("`== null` cannot be used under `not`".into()),
                    _ => Err("only `field == null` can test for null; `!= null` and ordering are never true".into()),
                };
            }
            let v = literal(lit).ok_or("a comparison needs a literal")?;
            Ok(json!({"field": f, "op": name, "value": v}))
        }
        _ => Err("only comparisons, `in`, `and`, `or` and `not` are allowed".into()),
    }
}

/// Checks a translated filter against the entity's fields the way the query planner will: the
/// field must exist and be filterable, ordering needs an orderable type, and a literal must fit
/// the field's type (and be one of an enum's values).
pub fn check_types(filter: &Value, fields: &serde_json::Map<String, Value>) -> Result<(), String> {
    if let Some(list) = filter
        .get("and")
        .or_else(|| filter.get("or"))
        .and_then(Value::as_array)
    {
        return list.iter().try_for_each(|f| check_types(f, fields));
    }
    if let Some(inner) = filter.get("not") {
        return check_types(inner, fields);
    }
    let name = filter["field"].as_str().unwrap_or_default();
    let op = filter["op"].as_str().unwrap_or_default();
    let def = fields
        .get(name)
        .ok_or_else(|| format!("no field `{name}`"))?;
    let ty = def["type"].as_str().unwrap_or_default();
    if matches!(ty, "json" | "file") {
        return Err(format!("`{name}` ({ty}) cannot be filtered"));
    }
    if op == "null" {
        return Ok(());
    }
    if matches!(op, "lt" | "lte" | "gt" | "gte") && matches!(ty, "boolean" | "enum" | "ref") {
        return Err(format!("`{name}` ({ty}) cannot be ordered"));
    }
    let fits = |v: &Value| match ty {
        "string" | "text" | "date" | "datetime" | "ref" => v.is_string(),
        "enum" => v.as_str().is_some_and(|s| {
            def["values"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x == s))
        }),
        "integer" => v.is_i64(),
        "decimal" => v.is_number() || v.as_str().is_some_and(|s| s.parse::<f64>().is_ok()),
        "boolean" => v.is_boolean(),
        _ => false,
    };
    let values: Vec<&Value> = match filter["value"].as_array() {
        Some(a) if op == "in" => a.iter().collect(),
        _ => vec![&filter["value"]],
    };
    match values.iter().find(|v| !fits(v)) {
        Some(v) => Err(format!("{v} does not fit `{name}` ({ty})")),
        None => Ok(()),
    }
}

/// The JSON filter for a projection's `filter` expression.
pub fn filter_json(src: &str) -> Result<Value, String> {
    let e = parse(src).map_err(|e| format!("{e:?}"))?;
    node(&e, false)
}

#[cfg(test)]
mod tests {
    use super::{check_types, filter_json};
    use serde_json::json;

    #[test]
    fn comparisons_lists_and_logic_translate() {
        assert_eq!(
            filter_json("status == 'open' and not (qty < 3 or tag in ['a', 'b'])").unwrap(),
            json!({"and": [
                {"field": "status", "op": "eq", "value": "open"},
                {"not": {"or": [
                    {"field": "qty", "op": "lt", "value": 3},
                    {"field": "tag", "op": "in", "value": ["a", "b"]}]}}]})
        );
        // A literal on the left flips the operator; null maps to the null test.
        assert_eq!(
            filter_json("3 < qty").unwrap(),
            json!({"field": "qty", "op": "gt", "value": 3})
        );
        assert_eq!(
            filter_json("note == null").unwrap(),
            json!({"field": "note", "op": "null", "value": true})
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "qty + 1 > 3",
            "ref(x).y == 1",
            "a == b",
            "qty < null",
            "note != null",
            "not (note == null)",
        ] {
            assert!(filter_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn types_are_checked_against_fields() {
        let fields = json!({
            "qty": {"type": "integer"},
            "on": {"type": "boolean"},
            "meta": {"type": "json"},
            "kind": {"type": "enum", "values": ["a", "b"]}
        });
        let f = fields.as_object().unwrap();
        let ok = |s: &str| check_types(&filter_json(s).unwrap(), f);
        assert!(ok("qty > 3").is_ok());
        assert!(ok("kind == 'a'").is_ok());
        assert!(ok("on > true").is_err());
        assert!(ok("meta == 'x'").is_err());
        assert!(ok("kind == 'z'").is_err());
        assert!(ok("qty == 'x'").is_err());
        assert!(ok("nope == 1").is_err());
    }
}
