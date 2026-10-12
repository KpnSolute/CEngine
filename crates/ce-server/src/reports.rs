//! Declared reports: a manifest `reports` entry groups one entity's records and computes
//! measures over each group (count, sum, avg, min, max). `GET .../_report/{name}` answers JSON,
//! CSV or a print-ready HTML page, for callers who can read the entity. The tenant, the soft
//! delete rule and the field types come from the same planner as the list.

use crate::Problem;
use axum::http::StatusCode;
use ce_manifest::Manifest;
use ce_store::{AggFn, AggRequest, Measure};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub(crate) fn exists(m: &Manifest, name: &str) -> bool {
    m.value["reports"][name].is_object()
}

pub(crate) fn entity(m: &Manifest, name: &str) -> String {
    m.value["reports"][name]["entity"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn bad_param(name: &str, why: &str) -> Problem {
    Problem::validation(
        format!("{name}: {why}"),
        vec![json!({"path": format!("/{name}"), "code": "invalid", "detail": why})],
    )
}

/// The aggregate request for a report: its declared filter, narrowed by any declared `params`
/// the caller passed (`?period=...` means `period == value`).
pub(crate) fn request(
    m: &Manifest,
    name: &str,
    q: &HashMap<String, String>,
) -> Result<AggRequest, Problem> {
    let r = &m.value["reports"][name];
    let fields = &m.value["entities"][r["entity"].as_str().unwrap_or_default()]["fields"];
    let strs = |k: &str| -> Vec<String> {
        r[k].as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    };
    let mut parts: Vec<Value> = Vec::new();
    if let Some(e) = r["filter"].as_str() {
        let f = ce_manifest::filter_json(e).map_err(|_| Problem::not_found())?;
        parts.push(f);
    }
    for p in strs("params") {
        let Some(v) = q.get(&p) else { continue };
        let value = match fields[p.as_str()]["type"].as_str() {
            Some("integer") => json!(v
                .parse::<i64>()
                .map_err(|_| bad_param(&p, "a whole number"))?),
            Some("boolean") => match v.as_str() {
                "true" => json!(true),
                "false" => json!(false),
                _ => return Err(bad_param(&p, "true or false")),
            },
            _ => json!(v),
        };
        parts.push(json!({"field": p, "op": "eq", "value": value}));
    }
    let filter = match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(json!({"and": parts})),
    };
    let measures = r["measures"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|ms| Measure {
            func: match ms["fn"].as_str() {
                Some("sum") => AggFn::Sum,
                Some("avg") => AggFn::Avg,
                Some("min") => AggFn::Min,
                Some("max") => AggFn::Max,
                _ => AggFn::Count,
            },
            field: ms["field"].as_str().map(String::from),
            name: ms["as"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    Ok(AggRequest {
        group_by: strs("group_by"),
        measures,
        filter,
    })
}

/// Column names in result order, each with whether it holds free text (group fields of type
/// string or enum), which decides the CSV formula guard.
pub(crate) fn columns(m: &Manifest, name: &str) -> Vec<(String, bool)> {
    let r = &m.value["reports"][name];
    let fields = &m.value["entities"][r["entity"].as_str().unwrap_or_default()]["fields"];
    let mut out: Vec<(String, bool)> = r["group_by"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|g| {
            let textual = matches!(fields[g]["type"].as_str(), Some("string" | "enum"));
            (g.to_string(), textual)
        })
        .collect();
    for ms in r["measures"].as_array().into_iter().flatten() {
        out.push((ms["as"].as_str().unwrap_or_default().to_string(), false));
    }
    out
}

pub(crate) fn title(m: &Manifest, name: &str) -> String {
    m.value["reports"][name]["title"]
        .as_str()
        .unwrap_or(name)
        .to_string()
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn rows_as_cells(
    cols: &[(String, bool)],
    rows: &[Map<String, Value>],
) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|r| {
            cols.iter()
                .map(|(c, _)| r.get(c).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// A standalone, print-ready page: every value is escaped, there is no script, and the page
/// is served with a policy that forbids loading anything.
pub(crate) fn html(
    title: &str,
    generated: &str,
    cols: &[(String, bool)],
    rows: &[Vec<Value>],
    truncated: bool,
) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    out.push_str(&format!("<title>{}</title>", escape(title)));
    out.push_str(
        "<style>body{font:14px/1.4 system-ui,sans-serif;margin:2rem;color:#111}\
         table{border-collapse:collapse;width:100%}th,td{border:1px solid #999;padding:.3rem .5rem;text-align:left}\
         th{background:#eee}td.n{text-align:right;font-variant-numeric:tabular-nums}\
         .meta{color:#555;margin-bottom:1rem}\
         @media print{body{margin:0}th{background:#ddd;-webkit-print-color-adjust:exact}}</style></head><body>",
    );
    out.push_str(&format!("<h1>{}</h1>", escape(title)));
    out.push_str(&format!(
        "<p class=\"meta\">Generated {}",
        escape(generated)
    ));
    if truncated {
        out.push_str(" &middot; only the first groups are shown");
    }
    out.push_str("</p><table><thead><tr>");
    for (c, _) in cols {
        out.push_str(&format!("<th>{}</th>", escape(c)));
    }
    out.push_str("</tr></thead><tbody>");
    for r in rows {
        out.push_str("<tr>");
        for (i, v) in r.iter().enumerate() {
            let numeric = cols.get(i).is_some_and(|c| !c.1) && !matches!(v, Value::Null);
            out.push_str(&format!(
                "<td{}>{}</td>",
                if numeric { " class=\"n\"" } else { "" },
                escape(&cell(v))
            ));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></body></html>");
    out
}

pub(crate) fn unsupported() -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "this store cannot compute reports",
    )
}
