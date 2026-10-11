//! CEngine import and export (draft v1): CSV in and out of an entity's records.
//!
//! Pure and synchronous. Export turns records into CSV text; import turns CSV text into
//! validated create bodies and a report. Nothing here touches storage: the caller creates the
//! accepted rows through the normal write path, so policy, invariants and computed fields apply
//! to imported data exactly as to any other write.
//!
//! Export: header is `id`, then the state field if the template has one, then the declared
//! fields in name order (computed fields included). Cells are quoted when they contain a comma,
//! quote or line break. Text that a spreadsheet could run as a formula (starts with `=`, `+`,
//! `-`, `@`, tab or carriage return) gets a leading `'`.
//!
//! Import: RFC 4180 CSV, UTF-8, optional byte-order mark, `\n` or `\r\n` line ends. The header
//! may name only declared fields that callers may set, each once; anything else rejects the
//! whole file. An empty cell means "not provided". A leading `'` before a formula character is
//! removed, undoing the export guard. Each row is converted by field type and then checked by
//! `ce_validate::validate` in create mode; rows are reported by their 1-based data row number.
//! Blank lines are skipped. Files over [`MAX_BYTES`] or [`MAX_ROWS`] are refused before parsing.

use ce_manifest::Manifest;
use ce_validate::{validate, Mode, RESERVED};
use serde_json::{Map, Value};

pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_ROWS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError {
    /// Column name, or `""` for the file as a whole.
    pub column: String,
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowReport {
    /// 1-based data row, not counting the header.
    pub row: usize,
    pub body: Map<String, Value>,
    /// Empty when the row is accepted.
    pub errors: Vec<IoError>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportReport {
    pub rows: Vec<RowReport>,
}

impl ImportReport {
    pub fn accepted(&self) -> impl Iterator<Item = &RowReport> {
        self.rows.iter().filter(|r| r.errors.is_empty())
    }
    pub fn rejected(&self) -> impl Iterator<Item = &RowReport> {
        self.rows.iter().filter(|r| !r.errors.is_empty())
    }
}

fn file_error(code: &'static str, detail: impl Into<String>) -> Vec<IoError> {
    vec![IoError {
        column: String::new(),
        code,
        detail: detail.into(),
    }]
}

fn dangerous(s: &str) -> bool {
    matches!(s.chars().next(), Some('=' | '+' | '-' | '@' | '\t' | '\r'))
}

fn quote(cell: &str) -> String {
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

/// The export columns for `entity`, or `None` if the entity does not exist.
pub fn columns(m: &Manifest, entity: &str) -> Option<Vec<String>> {
    let ent = &m.value["entities"][entity];
    let fields = ent["fields"].as_object()?;
    let mut cols = vec!["id".to_string()];
    if let Some(s) = ent["state"]["field"].as_str() {
        if !fields.contains_key(s) {
            cols.push(s.to_string());
        }
    }
    cols.extend(fields.keys().cloned());
    Some(cols)
}

/// Renders `records` as CSV. Fails only if the entity does not exist.
pub fn export_csv(m: &Manifest, entity: &str, records: &[Value]) -> Result<String, IoError> {
    let cols = columns(m, entity).ok_or_else(|| IoError {
        column: String::new(),
        code: "unknown_entity",
        detail: format!("no entity {entity}"),
    })?;
    let fields = &m.value["entities"][entity]["fields"];
    let mut out = cols.iter().map(|c| quote(c)).collect::<Vec<_>>().join(",");
    out.push_str("\r\n");
    for r in records {
        let row: Vec<String> = cols
            .iter()
            .map(|c| {
                let text = match &r[c.as_str()] {
                    Value::Null => String::new(),
                    Value::String(s) => s.clone(),
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    other => other.to_string(),
                };
                let textual = matches!(
                    fields[c.as_str()]["type"].as_str(),
                    Some("string" | "text" | "enum") | None
                );
                if textual && dangerous(&text) {
                    quote(&format!("'{text}"))
                } else {
                    quote(&text)
                }
            })
            .collect();
        out.push_str(&row.join(","));
        out.push_str("\r\n");
    }
    Ok(out)
}

/// Splits CSV text into rows of cells. Errors on an unterminated quote.
fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, IoError> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut in_quotes = false;
    let mut cell_started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() && !cell_started => {
                in_quotes = true;
                cell_started = true;
            }
            ',' => {
                row.push(std::mem::take(&mut cell));
                cell_started = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut cell));
                cell_started = false;
                rows.push(std::mem::take(&mut row));
            }
            _ => {
                cell.push(c);
                cell_started = true;
            }
        }
    }
    if in_quotes {
        return Err(IoError {
            column: String::new(),
            code: "bad_csv",
            detail: "a quoted cell is never closed".into(),
        });
    }
    if cell_started || !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    Ok(rows)
}

fn convert(field: &Value, cell: &str) -> Result<Value, &'static str> {
    let unguarded = match cell.strip_prefix('\'') {
        Some(rest) if dangerous(rest) => rest,
        _ => cell,
    };
    Ok(match field["type"].as_str().unwrap_or("") {
        "integer" => match cell.trim().parse::<i64>() {
            Ok(n) => Value::from(n),
            Err(_) => Value::String(cell.to_string()),
        },
        "boolean" => match cell.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(cell.to_string()),
        },
        "json" | "file" => serde_json::from_str(cell).map_err(|_| "not valid JSON")?,
        "decimal" | "date" | "datetime" | "ref" => Value::String(cell.trim().to_string()),
        _ => Value::String(unguarded.to_string()),
    })
}

/// Parses and checks `text` as a CSV of `entity` records.
pub fn import_csv(m: &Manifest, entity: &str, text: &str) -> Result<ImportReport, Vec<IoError>> {
    let ent = &m.value["entities"][entity];
    let Some(fields) = ent["fields"].as_object() else {
        return Err(file_error("unknown_entity", format!("no entity {entity}")));
    };
    if text.len() > MAX_BYTES {
        return Err(file_error("too_large", format!("over {MAX_BYTES} bytes")));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rows = parse_csv(text).map_err(|e| vec![e])?;
    let mut it = rows.into_iter();
    let Some(header) = it.next() else {
        return Err(file_error("empty", "the file has no header row"));
    };
    let state_field = ent["state"]["field"].as_str();
    let mut problems = Vec::new();
    for (i, h) in header.iter().enumerate() {
        let code = if h.is_empty() {
            Some("unnamed_column")
        } else if header[..i].contains(h) {
            Some("duplicate_column")
        } else if RESERVED.contains(&h.as_str()) {
            Some("reserved_field")
        } else if state_field == Some(h.as_str()) {
            Some("state_field")
        } else if !fields.contains_key(h) {
            Some("unknown_field")
        } else if fields[h]["computed"].is_string() {
            Some("computed_field")
        } else {
            None
        };
        if let Some(code) = code {
            problems.push(IoError {
                column: h.clone(),
                code,
                detail: "this column cannot be imported".into(),
            });
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let data: Vec<Vec<String>> = it.filter(|r| !(r.len() == 1 && r[0].is_empty())).collect();
    if data.len() > MAX_ROWS {
        return Err(file_error("too_many_rows", format!("over {MAX_ROWS} rows")));
    }
    let mut out = Vec::new();
    for (n, cells) in data.into_iter().enumerate() {
        let mut body = Map::new();
        let mut errors = Vec::new();
        if cells.len() != header.len() {
            errors.push(IoError {
                column: String::new(),
                code: "column_count",
                detail: format!("expected {} cells, found {}", header.len(), cells.len()),
            });
        } else {
            for (h, cell) in header.iter().zip(&cells) {
                if cell.is_empty() {
                    continue;
                }
                match convert(&fields[h], cell) {
                    Ok(v) => {
                        body.insert(h.clone(), v);
                    }
                    Err(detail) => errors.push(IoError {
                        column: h.clone(),
                        code: "invalid_format",
                        detail: detail.into(),
                    }),
                }
            }
            if errors.is_empty() {
                if let Err(es) = validate(m, entity, &body, Mode::Create) {
                    errors.extend(es.into_iter().map(|e| IoError {
                        column: e.path.trim_start_matches('/').to_string(),
                        code: e.code,
                        detail: e.detail,
                    }));
                }
            }
        }
        out.push(RowReport {
            row: n + 1,
            body,
            errors,
        });
    }
    Ok(ImportReport { rows: out })
}
