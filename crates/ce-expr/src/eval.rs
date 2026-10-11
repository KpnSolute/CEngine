use crate::parse::{Expr, Op};
use crate::value::{digits, Value, MAX_DIGITS};
use bigdecimal::{BigDecimal, RoundingMode, Zero};
use std::collections::HashMap;

/// Bindings: top-level names. Nested objects are not modelled yet; a dotted
/// path is looked up as the flat key `a.b`.
pub type Env = HashMap<String, Value>;

#[derive(Debug, PartialEq)]
pub enum EvalError {
    Type(String),
    Overflow,
    UnknownFunction(String),
    Arity(String),
    Aggregate(String),
}

type R = Result<Value, EvalError>;

fn ty(msg: &str) -> EvalError {
    EvalError::Type(msg.to_string())
}

/// Round half-up, away from zero on ties (spec).
fn round_to(d: &BigDecimal, n: i64) -> BigDecimal {
    d.with_scale_round(n, RoundingMode::HalfUp)
}

fn check(d: BigDecimal) -> R {
    if digits(&d) > MAX_DIGITS || d.fractional_digit_count() > 18 {
        Err(EvalError::Overflow)
    } else {
        Ok(Value::Dec(d))
    }
}

/// Reads other records for `ref(field).attr` and `sum`/`count` over `children(entity, field)`.
/// The caller binds it to one record and one tenant, so expressions never name either.
/// An `Err` is the reason the lookup failed; evaluation reports it as `EvalError::Aggregate`.
pub trait Resolver {
    /// The `attr` of the row that the ref field `field` points to (`id`), or null if there is none.
    fn ref_attr(&self, field: &str, id: &str, attr: &str) -> Result<Value, String>;
    /// `attr` of every live row of `entity` whose `ref_field` points at the current record.
    /// With no `attr`, one non-null value per row. Nulls may be included; callers skip them.
    fn children(
        &self,
        entity: &str,
        ref_field: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, String>;
}

/// No store: every lookup fails, so `ref` and aggregates stay unsupported.
pub struct NoResolver;

impl Resolver for NoResolver {
    fn ref_attr(&self, _: &str, _: &str, _: &str) -> Result<Value, String> {
        Err("ref needs the store".into())
    }
    fn children(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<Value>, String> {
        Err("children needs the store".into())
    }
}

pub fn eval(e: &Expr, env: &Env) -> R {
    eval_with(e, env, &NoResolver)
}

fn plain(e: &Expr) -> Option<&str> {
    match e {
        Expr::Path(p) if p.len() == 1 => Some(p[0].as_str()),
        _ => None,
    }
}

/// `children(entity, field)` possibly followed by `.attr`.
fn children_source(e: &Expr) -> Option<(&str, &str, Option<&str>)> {
    let (call, attr) = match e {
        Expr::Field(base, attr) => (&**base, Some(attr.as_str())),
        other => (other, None),
    };
    match call {
        Expr::Call(name, args) if name == "children" && args.len() == 2 => {
            Some((plain(&args[0])?, plain(&args[1])?, attr))
        }
        _ => None,
    }
}

fn lookup<T>(name: &str, r: Result<T, String>) -> Result<T, EvalError> {
    r.map_err(|_| EvalError::Aggregate(name.to_string()))
}

fn sum(values: Vec<Value>) -> R {
    let mut int: i64 = 0;
    let mut dec: Option<BigDecimal> = None;
    for v in values {
        match v {
            Value::Null => {}
            Value::Int(i) => match &mut dec {
                Some(d) => *d += BigDecimal::from(i),
                None => int = int.checked_add(i).ok_or(EvalError::Overflow)?,
            },
            Value::Dec(x) => {
                let base = dec.take().unwrap_or_else(|| BigDecimal::from(int));
                dec = Some(base + x);
            }
            _ => return Err(ty("sum needs numbers")),
        }
    }
    match dec {
        Some(d) => check(d),
        None => Ok(Value::Int(int)),
    }
}

pub fn eval_with(e: &Expr, env: &Env, r: &dyn Resolver) -> R {
    let eval = |x: &Expr, env: &Env| eval_with(x, env, r);
    match e {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Int(i) => Ok(Value::Int(*i)),
        Expr::Dec(d) => Ok(Value::Dec(d.clone())),
        Expr::Str(s) => Ok(Value::Str(s.clone())),
        Expr::List(v) => Ok(Value::List(
            v.iter().map(|x| eval(x, env)).collect::<Result<_, _>>()?,
        )),
        Expr::Path(p) => Ok(env.get(&p.join(".")).cloned().unwrap_or(Value::Null)),
        Expr::Neg(x) => match eval(x, env)? {
            Value::Null => Ok(Value::Null),
            Value::Int(i) => i.checked_neg().map(Value::Int).ok_or(EvalError::Overflow),
            Value::Dec(d) => Ok(Value::Dec(-d)),
            _ => Err(ty("negation needs a number")),
        },
        Expr::Not(x) => match eval(x, env)? {
            Value::Null => Ok(Value::Null),
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err(ty("not needs a bool")),
        },
        Expr::Call(f, a) => call(f, a, env, r),
        Expr::Field(base, attr) if matches!(&**base, Expr::Call(n, a) if n == "ref" && a.len() == 1 && plain(&a[0]).is_some()) =>
        {
            let Expr::Call(_, a) = &**base else {
                unreachable!()
            };
            let field = plain(&a[0]).unwrap_or_default();
            match env.get(field) {
                None | Some(Value::Null) => Ok(Value::Null),
                Some(Value::Str(id)) => lookup("ref", r.ref_attr(field, id, attr)),
                Some(_) => Err(ty("ref needs a ref field")),
            }
        }
        Expr::Field(base, _) => match eval(base, env)? {
            Value::Null => Ok(Value::Null),
            _ => Err(ty(
                "field access on a value needs the store (not available here)",
            )),
        },
        Expr::Bin(op, a, b) => bin(*op, a, b, env, r),
    }
}

fn tri(v: &Value) -> Result<Option<bool>, EvalError> {
    match v {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        _ => Err(ty("expected a bool")),
    }
}

fn bin(op: Op, a: &Expr, b: &Expr, env: &Env, r: &dyn Resolver) -> R {
    let av = eval_with(a, env, r)?;
    let bv = eval_with(b, env, r)?;
    let boolv = |o: Option<bool>| o.map_or(Value::Null, Value::Bool);
    match op {
        Op::Or => {
            let (x, y) = (tri(&av)?, tri(&bv)?);
            Ok(boolv(match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (None, _) | (_, None) => None,
                _ => Some(false),
            }))
        }
        Op::And => {
            let (x, y) = (tri(&av)?, tri(&bv)?);
            Ok(boolv(match (x, y) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (None, _) | (_, None) => None,
                _ => Some(true),
            }))
        }
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge => {
            if av == Value::Null && bv == Value::Null {
                return Ok(match op {
                    Op::Eq => Value::Bool(true),
                    Op::Ne => Value::Bool(false),
                    _ => Value::Null,
                });
            }
            if av == Value::Null || bv == Value::Null {
                return Ok(Value::Null);
            }
            match op {
                Op::Eq | Op::Ne => {
                    let eq = av
                        .equals(&bv)
                        .ok_or_else(|| ty("cannot compare these types"))?;
                    Ok(Value::Bool(if op == Op::Eq { eq } else { !eq }))
                }
                _ => {
                    let o = av
                        .compare(&bv)
                        .ok_or_else(|| ty("cannot order these types"))?;
                    Ok(Value::Bool(match op {
                        Op::Lt => o.is_lt(),
                        Op::Le => o.is_le(),
                        Op::Gt => o.is_gt(),
                        _ => o.is_ge(),
                    }))
                }
            }
        }
        Op::In => {
            if av == Value::Null {
                return Ok(Value::Null);
            }
            match bv {
                Value::List(items) => {
                    let mut hit = false;
                    for i in &items {
                        if i != &Value::Null
                            && av.equals(i).ok_or_else(|| ty("cannot compare in list"))?
                        {
                            hit = true;
                        }
                    }
                    Ok(Value::Bool(hit))
                }
                Value::Null => Ok(Value::Bool(false)),
                _ => Err(ty("right side of in must be a list")),
            }
        }
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Rem => arith(op, av, bv),
    }
}

fn arith(op: Op, a: Value, b: Value) -> R {
    if a == Value::Null || b == Value::Null {
        return Ok(Value::Null);
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let r = match op {
            Op::Add => x.checked_add(*y),
            Op::Sub => x.checked_sub(*y),
            Op::Mul => x.checked_mul(*y),
            Op::Rem => {
                if *y == 0 {
                    return Ok(Value::Null);
                }
                x.checked_rem(*y)
            }
            _ => None, // division handled below
        };
        if op != Op::Div {
            return r.map(Value::Int).ok_or(EvalError::Overflow);
        }
    }
    if op == Op::Rem {
        return Err(ty("% needs two ints"));
    }
    let (x, y) = match (a.as_dec(), b.as_dec()) {
        (Some(x), Some(y)) => (x, y),
        _ => return Err(ty("arithmetic needs numbers")),
    };
    match op {
        Op::Add => check(x + y),
        Op::Sub => check(x - y),
        Op::Mul => check(x * y),
        Op::Div => {
            if y.is_zero() {
                return Ok(Value::Null);
            }
            // Divide at high precision, then round half-up to scale 18.
            let q = (x / y).with_prec(80);
            check(round_to(&q, 18))
        }
        _ => unreachable!(),
    }
}

fn arity(name: &str, args: &[Value], n: usize) -> Result<(), EvalError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(EvalError::Arity(name.to_string()))
    }
}

fn call(name: &str, raw: &[Expr], env: &Env, r: &dyn Resolver) -> R {
    if matches!(name, "sum" | "count") && raw.len() == 1 {
        if let Some((entity, field, attr)) = children_source(&raw[0]) {
            let values = lookup(name, r.children(entity, field, attr))?;
            return if name == "sum" {
                sum(values)
            } else {
                Ok(Value::Int(
                    values.iter().filter(|v| **v != Value::Null).count() as i64,
                ))
            };
        }
    }
    if matches!(name, "sum" | "count" | "children" | "ref") {
        return Err(EvalError::Aggregate(name.to_string()));
    }
    let args: Vec<Value> = raw
        .iter()
        .map(|x| eval_with(x, env, r))
        .collect::<Result<_, _>>()?;
    match name {
        "round" => {
            arity(name, &args, 2)?;
            let n = match &args[1] {
                Value::Int(n) if (0..=18).contains(n) => *n,
                _ => return Err(ty("round scale must be an int 0..18")),
            };
            match &args[0] {
                Value::Null => Ok(Value::Null),
                v => match v.as_dec() {
                    Some(d) => check(round_to(&d, n)),
                    None => Err(ty("round needs a number")),
                },
            }
        }
        "coalesce" => Ok(args
            .into_iter()
            .find(|v| *v != Value::Null)
            .unwrap_or(Value::Null)),
        "when" => {
            arity(name, &args, 3)?;
            Ok(if args[0] == Value::Bool(true) {
                args[1].clone()
            } else {
                args[2].clone()
            })
        }
        "abs" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(i) => i.checked_abs().map(Value::Int).ok_or(EvalError::Overflow),
                Value::Dec(d) => Ok(Value::Dec(d.abs())),
                _ => Err(ty("abs needs a number")),
            }
        }
        "min" | "max" => {
            arity(name, &args, 2)?;
            if args.contains(&Value::Null) {
                return Ok(Value::Null);
            }
            let o = args[0]
                .compare(&args[1])
                .ok_or_else(|| ty("cannot order these types"))?;
            let first = if name == "min" { o.is_le() } else { o.is_ge() };
            Ok(if first {
                args[0].clone()
            } else {
                args[1].clone()
            })
        }
        "is_null" => {
            arity(name, &args, 1)?;
            Ok(Value::Bool(args[0] == Value::Null))
        }
        "len" | "lower" | "upper" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => Ok(match name {
                    "len" => Value::Int(s.chars().count() as i64),
                    "lower" => Value::Str(s.to_lowercase()),
                    _ => Value::Str(s.to_uppercase()),
                }),
                _ => Err(ty("expected a string")),
            }
        }
        "trim" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Str(s) => Ok(Value::Str(s.trim().to_string())),
                _ => Err(ty("expected a string")),
            }
        }
        "floor" | "ceil" => {
            arity(name, &args, 1)?;
            match &args[0] {
                Value::Null => Ok(Value::Null),
                Value::Int(i) => Ok(Value::Int(*i)),
                Value::Dec(d) => {
                    let mode = if name == "floor" {
                        RoundingMode::Floor
                    } else {
                        RoundingMode::Ceiling
                    };
                    check(d.with_scale_round(0, mode))
                }
                _ => Err(ty("expected a number")),
            }
        }
        "concat" => {
            let mut out = String::new();
            for a in &args {
                match a {
                    Value::Null => return Ok(Value::Null),
                    Value::Str(s) => out.push_str(s),
                    _ => return Err(ty("concat needs strings")),
                }
            }
            Ok(Value::Str(out))
        }
        "starts_with" | "contains" => {
            arity(name, &args, 2)?;
            match (&args[0], &args[1]) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(p)) => Ok(Value::Bool(if name == "contains" {
                    s.contains(p.as_str())
                } else {
                    s.starts_with(p.as_str())
                })),
                _ => Err(ty("expected strings")),
            }
        }
        other => Err(EvalError::UnknownFunction(other.to_string())),
    }
}
