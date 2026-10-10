use bigdecimal::BigDecimal;
use std::cmp::Ordering;

/// Maximum total digits of a decimal (spec: 38).
pub const MAX_DIGITS: u64 = 38;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Dec(BigDecimal),
    Str(String),
    List(Vec<Value>),
}

impl Value {
    /// Canonical output form used by the conformance vectors:
    /// ints and decimals as strings (decimals keep their scale), strings raw.
    pub fn canonical(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(b.to_string()),
            Value::Int(i) => Some(i.to_string()),
            Value::Dec(d) => Some(d.to_string()),
            Value::Str(s) => Some(s.clone()),
            Value::List(_) => Some("[list]".into()),
        }
    }

    pub(crate) fn as_dec(&self) -> Option<BigDecimal> {
        match self {
            Value::Int(i) => Some(BigDecimal::from(*i)),
            Value::Dec(d) => Some(d.clone()),
            _ => None,
        }
    }

    /// Equality across int and decimal; other types compare only with their own type.
    pub(crate) fn equals(&self, other: &Value) -> Option<bool> {
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => Some(a == b),
            (Value::Str(a), Value::Str(b)) => Some(a == b),
            (a, b) => match (a.as_dec(), b.as_dec()) {
                (Some(x), Some(y)) => Some(x == y),
                _ => None,
            },
        }
    }

    pub(crate) fn compare(&self, other: &Value) -> Option<Ordering> {
        match (self, other) {
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            (a, b) => match (a.as_dec(), b.as_dec()) {
                (Some(x), Some(y)) => Some(x.cmp(&y)),
                _ => None,
            },
        }
    }
}

/// Total significant digits of a decimal.
pub(crate) fn digits(d: &BigDecimal) -> u64 {
    d.digits()
}
