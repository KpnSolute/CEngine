//! CEngine record validation (draft v1): checks the body of a create or patch against the
//! manifest before anything touches storage. Pure and synchronous, so the server, the WASM
//! build and the Python bindings give the same answer.
//!
//! Rules, each tested:
//! 1. Callers may not set engine-owned names (`id`, `tenant_id`, `version`, timestamps),
//!    fields the template does not declare, or computed fields.
//! 2. Every non-computed required field must be present and not null on create. On patch a
//!    required field may be omitted (it keeps its value) but never set to null.
//! 3. `null` on an optional field means "remove" and is allowed only on patch.
//! 4. Types are strict: no string-to-number coercion, integers must be whole numbers that fit
//!    64 bits, decimals must fit the declared precision and scale (never silently rounded) and
//!    must not use exponent notation, dates are `YYYY-MM-DD`, datetimes are RFC 3339 with a
//!    zone, enums must be one of the declared values, refs must be UUIDs.
//! 5. `max_length` counts characters, not bytes. A NUL character is never allowed in text.
//!
//! Not here: invariants and computed values (they need the expression evaluator and the
//! stored record), uniqueness and reference existence (the store enforces those).

use ce_manifest::Manifest;
use chrono::{DateTime, NaiveDate};
use serde_json::{Map, Value};

/// Names the engine owns on every record; callers may never set them.
pub const RESERVED: [&str; 6] = [
    "id",
    "tenant_id",
    "version",
    "created_at",
    "updated_at",
    "deleted_at",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Create,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// JSON Pointer to the offending field, `""` for the body as a whole.
    pub path: String,
    pub code: &'static str,
    pub detail: String,
}

fn err(field: &str, code: &'static str, detail: impl Into<String>) -> FieldError {
    FieldError {
        path: format!("/{field}"),
        code,
        detail: detail.into(),
    }
}

/// Validates `body` for `entity`. Returns every problem found, in field-name order.
pub fn validate(
    m: &Manifest,
    entity: &str,
    body: &Map<String, Value>,
    mode: Mode,
) -> Result<(), Vec<FieldError>> {
    let ent = &m.value["entities"][entity];
    let Some(fields) = ent["fields"].as_object() else {
        return Err(vec![FieldError {
            path: String::new(),
            code: "unknown_entity",
            detail: format!("no entity {entity}"),
        }]);
    };
    let mut errors = Vec::new();
    let mut names: Vec<&String> = body.keys().collect();
    names.sort();
    for k in names {
        let v = &body[k];
        if RESERVED.contains(&k.as_str()) {
            errors.push(err(k, "reserved_field", "set by the engine"));
            continue;
        }
        let Some(f) = fields.get(k) else {
            errors.push(err(k, "unknown_field", "not declared by the template"));
            continue;
        };
        if f["computed"].is_string() {
            errors.push(err(k, "computed_field", "derived by the engine"));
            continue;
        }
        if v.is_null() {
            if mode == Mode::Create {
                errors.push(err(k, "null_not_allowed", "omit the field instead"));
            } else if f["required"] == true {
                errors.push(err(k, "required", "a required field cannot be removed"));
            }
            continue;
        }
        if let Some(e) = check_value(k, f, v) {
            errors.push(e);
        }
    }
    if mode == Mode::Create {
        let mut missing: Vec<&String> = fields
            .iter()
            .filter(|(k, f)| {
                f["required"] == true && !f["computed"].is_string() && !body.contains_key(*k)
            })
            .map(|(k, _)| k)
            .collect();
        missing.sort();
        for k in missing {
            errors.push(err(k, "required", "this field is required"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn mismatch(k: &str, want: &str) -> Option<FieldError> {
    Some(err(k, "type_mismatch", format!("expected {want}")))
}

fn check_value(k: &str, f: &Value, v: &Value) -> Option<FieldError> {
    match f["type"].as_str().unwrap_or("") {
        "string" | "text" => {
            let Some(s) = v.as_str() else {
                return mismatch(k, "a string");
            };
            if s.contains('\0') {
                return Some(err(k, "invalid_value", "NUL characters are not allowed"));
            }
            if let Some(max) = f["max_length"].as_u64() {
                if s.chars().count() as u64 > max {
                    return Some(err(k, "too_long", format!("at most {max} characters")));
                }
            }
            None
        }
        "integer" => match v {
            Value::Number(n) if n.is_i64() => None,
            Value::Number(n) if n.is_u64() => {
                Some(err(k, "out_of_range", "does not fit a 64-bit integer"))
            }
            _ => mismatch(k, "a whole number"),
        },
        "decimal" => check_decimal(k, f, v),
        "boolean" => v
            .is_boolean()
            .then_some(())
            .map_or_else(|| mismatch(k, "true or false"), |_| None),
        "date" => {
            let Some(s) = v.as_str() else {
                return mismatch(k, "a date string");
            };
            if s.len() == 10 && NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
                None
            } else {
                Some(err(k, "invalid_format", "expected YYYY-MM-DD"))
            }
        }
        "datetime" => {
            let Some(s) = v.as_str() else {
                return mismatch(k, "a datetime string");
            };
            if DateTime::parse_from_rfc3339(s).is_ok() {
                None
            } else {
                Some(err(
                    k,
                    "invalid_format",
                    "expected RFC 3339 with a time zone",
                ))
            }
        }
        "enum" => {
            let Some(s) = v.as_str() else {
                return mismatch(k, "a string");
            };
            let ok = f["values"]
                .as_array()
                .is_some_and(|vals| vals.iter().any(|x| x.as_str() == Some(s)));
            if ok {
                None
            } else {
                Some(err(k, "invalid_value", "not one of the allowed values"))
            }
        }
        "ref" => {
            let Some(s) = v.as_str() else {
                return mismatch(k, "a UUID string");
            };
            if is_uuid(s) {
                None
            } else {
                Some(err(k, "invalid_format", "expected a UUID"))
            }
        }
        "file" => v
            .is_object()
            .then_some(())
            .map_or_else(|| mismatch(k, "an object"), |_| None),
        // json: any value
        _ => None,
    }
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn check_decimal(k: &str, f: &Value, v: &Value) -> Option<FieldError> {
    let text = match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return mismatch(k, "a number or a decimal string"),
    };
    let body = text.strip_prefix('-').unwrap_or(&text);
    let (int, frac) = match body.split_once('.') {
        Some((i, fr)) => (i, fr),
        None => (body, ""),
    };
    let digits = |s: &str| s.bytes().all(|c| c.is_ascii_digit());
    let well_formed =
        !int.is_empty() && digits(int) && digits(frac) && (body.contains('.') == !frac.is_empty());
    if !well_formed {
        return Some(err(
            k,
            "invalid_format",
            "expected plain decimal digits (no exponent); send big or tiny values as a string",
        ));
    }
    let precision = f["precision"].as_u64().unwrap_or(38) as usize;
    let scale = f["scale"].as_u64().unwrap_or(0) as usize;
    if frac.len() > scale {
        return Some(err(
            k,
            "out_of_range",
            format!("at most {scale} digits after the point"),
        ));
    }
    let int_significant = int.trim_start_matches('0').len();
    if int_significant > precision.saturating_sub(scale) {
        return Some(err(
            k,
            "out_of_range",
            format!(
                "at most {} digits before the point",
                precision.saturating_sub(scale)
            ),
        ));
    }
    None
}
