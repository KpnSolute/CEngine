use bigdecimal::BigDecimal;
use ce_expr::{run, run_with, Env, Error, EvalError, Resolver, Value};
use std::str::FromStr;

fn dec(s: &str) -> Value {
    Value::Dec(BigDecimal::from_str(s).unwrap())
}

/// Two child rows `line(parent)` with `amount` 1.10 and 2.20 and `qty` 2 and null,
/// and one referenced row whose `state` is 'open'.
struct Fake {
    fail: bool,
}

impl Resolver for Fake {
    fn ref_attr(&self, field: &str, id: &str, attr: &str) -> Result<Value, String> {
        if self.fail {
            return Err("down".into());
        }
        assert_eq!((field, id), ("owner", "o1"));
        Ok(match attr {
            "state" => Value::Str("open".into()),
            "limit" => Value::Int(10),
            _ => Value::Null,
        })
    }
    fn children(
        &self,
        entity: &str,
        ref_field: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, String> {
        if self.fail {
            return Err("down".into());
        }
        assert_eq!((entity, ref_field), ("line", "parent"));
        Ok(match attr {
            Some("amount") => vec![dec("1.10"), dec("2.20")],
            Some("qty") => vec![Value::Int(2), Value::Null],
            Some("mixed") => vec![Value::Int(1), dec("0.5"), Value::Int(2)],
            Some("big") => vec![Value::Int(i64::MAX), Value::Int(1)],
            Some("wide") => vec![Value::Int(i64::MAX), Value::Int(1), dec("0.0")],
            Some("back") => vec![Value::Int(i64::MAX), Value::Int(1), Value::Int(-1)],
            Some("name") => vec![Value::Str("a".into())],
            None => vec![Value::Bool(true), Value::Bool(true)],
            _ => vec![],
        })
    }
}

fn eval(src: &str, env: &Env) -> Result<Value, Error> {
    run_with(src, env, &Fake { fail: false })
}

#[test]
fn sums_and_counts_over_children() {
    let env = Env::new();
    assert_eq!(
        eval("sum(children(line, parent).amount)", &env),
        Ok(dec("3.30"))
    );
    assert_eq!(
        eval("sum(children(line, parent).qty)", &env),
        Ok(Value::Int(2))
    );
    assert_eq!(
        eval("sum(children(line, parent).mixed)", &env),
        Ok(dec("3.5"))
    );
    assert_eq!(
        eval("sum(children(line, parent).nothing)", &env),
        Ok(Value::Int(0))
    );
    assert_eq!(
        eval("count(children(line, parent))", &env),
        Ok(Value::Int(2))
    );
    assert_eq!(
        eval("count(children(line, parent).qty)", &env),
        Ok(Value::Int(1))
    );
    // Aggregates compose with the rest of the language.
    assert_eq!(
        eval("round(sum(children(line, parent).amount) * 2, 1)", &env),
        Ok(dec("6.6"))
    );
    assert_eq!(
        eval("sum(children(line, parent).amount) == 3.3", &env),
        Ok(Value::Bool(true))
    );
}

#[test]
fn bad_aggregates_are_errors_not_zero() {
    let env = Env::new();
    assert_eq!(
        eval("sum(children(line, parent).big)", &env),
        Err(Error::Eval(EvalError::Overflow))
    );
    // The order of rows never changes the answer: the limits apply to the final sum.
    assert_eq!(
        eval("sum(children(line, parent).wide)", &env),
        Ok(dec("9223372036854775808.0"))
    );
    assert_eq!(
        eval("sum(children(line, parent).back)", &env),
        Ok(Value::Int(i64::MAX))
    );
    assert!(matches!(
        eval("sum(children(line, parent).name)", &env),
        Err(Error::Eval(EvalError::Type(_)))
    ));
    // Aggregates over anything but children() stay unsupported.
    assert_eq!(
        eval("sum([1, 2])", &env),
        Err(Error::Eval(EvalError::Aggregate("sum".into())))
    );
    assert_eq!(
        eval("count(children(line, parent), 1)", &env),
        Err(Error::Eval(EvalError::Aggregate("count".into())))
    );
}

#[test]
fn ref_reads_one_attribute_of_the_referenced_row() {
    let mut env = Env::new();
    env.insert("owner".into(), Value::Str("o1".into()));
    assert_eq!(
        eval("ref(owner).state == 'open'", &env),
        Ok(Value::Bool(true))
    );
    assert_eq!(eval("ref(owner).limit + 1", &env), Ok(Value::Int(11)));
    assert_eq!(eval("ref(owner).missing", &env), Ok(Value::Null));
    // A null ref gives null without asking the store.
    env.insert("owner".into(), Value::Null);
    assert_eq!(eval("ref(owner).state", &env), Ok(Value::Null));
    assert_eq!(eval("ref(unbound).state", &Env::new()), Ok(Value::Null));
    // Not a ref value.
    env.insert("owner".into(), Value::Int(3));
    assert!(matches!(
        eval("ref(owner).state", &env),
        Err(Error::Eval(EvalError::Type(_)))
    ));
}

#[test]
fn lookup_failures_surface_and_the_plain_evaluator_still_refuses() {
    let mut env = Env::new();
    env.insert("owner".into(), Value::Str("o1".into()));
    let down = Fake { fail: true };
    assert_eq!(
        run_with("ref(owner).state", &env, &down),
        Err(Error::Eval(EvalError::Aggregate("ref".into())))
    );
    assert_eq!(
        run_with("count(children(line, parent))", &env, &down),
        Err(Error::Eval(EvalError::Aggregate("count".into())))
    );
    assert_eq!(
        run("sum(children(line, parent).amount)", &env),
        Err(Error::Eval(EvalError::Aggregate("sum".into())))
    );
    assert_eq!(
        run("ref(owner).state", &env),
        Err(Error::Eval(EvalError::Aggregate("ref".into())))
    );
}
