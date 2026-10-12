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

fn node(e: &Expr) -> Result<Value, String> {
    match e {
        Expr::Not(inner) => Ok(json!({"not": node(inner)?})),
        Expr::Bin(Op::And, a, b) => Ok(json!({"and": [node(a)?, node(b)?]})),
        Expr::Bin(Op::Or, a, b) => Ok(json!({"or": [node(a)?, node(b)?]})),
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
                    Op::Eq => Ok(json!({"field": f, "op": "null", "value": true})),
                    Op::Ne => Ok(json!({"field": f, "op": "null", "value": false})),
                    _ => Err("null can only be compared with == or !=".into()),
                };
            }
            let v = literal(lit).ok_or("a comparison needs a literal")?;
            Ok(json!({"field": f, "op": name, "value": v}))
        }
        _ => Err("only comparisons, `in`, `and`, `or` and `not` are allowed".into()),
    }
}

/// The JSON filter for a projection's `filter` expression.
pub fn filter_json(src: &str) -> Result<Value, String> {
    let e = parse(src).map_err(|e| format!("{e:?}"))?;
    node(&e)
}

#[cfg(test)]
mod tests {
    use super::filter_json;
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
            filter_json("note != null").unwrap(),
            json!({"field": "note", "op": "null", "value": false})
        );
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "qty + 1 > 3",
            "ref(x).y == 1",
            "a == b",
            "today > '2026'",
            "qty < null",
        ] {
            assert!(filter_json(bad).is_err(), "{bad}");
        }
    }
}
