use crate::{filter, TenantScope};
use ce_manifest::Manifest;
use serde_json::Value;

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
pub const MAX_SEARCH_LEN: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Text(String),
    Int(i64),
    /// Exact decimal as a string; bound with a `::text::numeric` cast.
    Dec(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryError {
    UnknownEntity(String),
    UnknownField(String),
    NotFilterable(String),
    BadOp(String, String),
    BadValue(String),
    BadFilter(&'static str),
    FilterTooLarge,
    UnindexedSort(String),
    BadCursor,
    /// The entity declares no `searchable` field.
    NotSearchable(String),
    /// The search text is empty or too long.
    BadSearch,
    BadSchema,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone)]
pub struct SortKey {
    pub field: String,
    pub dir: SortDir,
}

/// Where the entity's table lives. `schema` is built by the caller from the installation.
#[derive(Debug, Clone)]
pub struct Target {
    pub schema: String,
    pub entity: String,
}

#[derive(Debug, Clone, Default)]
pub struct ListRequest {
    pub select: Option<Vec<String>>,
    pub filter: Option<Value>,
    /// Text to find in the entity's `searchable` fields (case-insensitive, anywhere in the
    /// field). At most [`MAX_SEARCH_LEN`] characters.
    pub search: Option<String>,
    pub sort: Vec<SortKey>,
    pub limit: Option<usize>,
    /// Values of the last row's sort keys, then its `id`, from the previous page.
    pub after: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub sql: String,
    /// The fields behind result columns 2.., in order. Columns 0 and 1 are `id` (text) and
    /// `version`. Decimal columns come back as text, every other column as `jsonb`.
    pub columns: Vec<String>,
    /// `$1` is always the tenant id.
    pub params: Vec<Param>,
}

pub(crate) struct Ctx<'a> {
    entity: &'a Value,
    params: Vec<Param>,
}

impl Ctx<'_> {
    pub fn bind(&mut self, p: Param) -> usize {
        self.params.push(p);
        self.params.len()
    }

    fn field(&self, name: &str) -> Result<&Value, QueryError> {
        self.entity["fields"]
            .get(name)
            .ok_or_else(|| QueryError::UnknownField(name.to_string()))
    }

    pub fn field_type(&self, name: &str) -> Result<String, QueryError> {
        if name == "id" {
            return Ok("ref".into());
        }
        if let Some(sf) = self.entity["state"]["field"].as_str() {
            if sf == name && self.entity["fields"].get(name).is_none() {
                return Ok("string".into());
            }
        }
        Ok(self.field(name)?["type"]
            .as_str()
            .unwrap_or("string")
            .to_string())
    }

    /// A quoted column. Names are only accepted if the manifest declares them.
    pub fn column(&self, name: &str) -> Result<String, QueryError> {
        self.field_type(name)?;
        Ok(format!("\"{name}\""))
    }

    /// Convert a JSON value to a bound parameter for `field`, with its SQL cast.
    pub fn value(&self, field: &str, v: &Value) -> Result<(Param, &'static str), QueryError> {
        let bad = || QueryError::BadValue(field.to_string());
        let ty = self.field_type(field)?;
        Ok(match ty.as_str() {
            "string" | "text" => (Param::Text(v.as_str().ok_or_else(bad)?.to_string()), ""),
            "integer" => (Param::Int(v.as_i64().ok_or_else(bad)?), ""),
            "boolean" => (Param::Bool(v.as_bool().ok_or_else(bad)?), ""),
            "decimal" => {
                let s = v.as_str().ok_or_else(bad)?;
                let ok = !s.is_empty()
                    && s.trim_start_matches('-')
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '.')
                    && s.matches('.').count() <= 1;
                if !ok {
                    return Err(bad());
                }
                (Param::Dec(s.to_string()), "::text::numeric")
            }
            "date" => (
                Param::Text(v.as_str().ok_or_else(bad)?.to_string()),
                "::text::date",
            ),
            "datetime" => (
                Param::Text(v.as_str().ok_or_else(bad)?.to_string()),
                "::text::timestamptz",
            ),
            "ref" => (
                Param::Text(v.as_str().ok_or_else(bad)?.to_string()),
                "::text::uuid",
            ),
            "enum" => {
                let s = v.as_str().ok_or_else(bad)?;
                let allowed = self.entity["fields"][field]["values"]
                    .as_array()
                    .map(|a| a.iter().any(|x| x == s))
                    .unwrap_or(true);
                if !allowed {
                    return Err(bad());
                }
                (Param::Text(s.to_string()), "")
            }
            _ => return Err(QueryError::NotFilterable(field.to_string())),
        })
    }
}

pub fn plan_list(
    m: &Manifest,
    t: &Target,
    scope: &TenantScope,
    req: &ListRequest,
) -> Result<Planned, QueryError> {
    let ident = !t.schema.is_empty()
        && t.schema.starts_with(|c: char| c.is_ascii_lowercase())
        && t.schema
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ident {
        return Err(QueryError::BadSchema);
    }
    let entity = m.value["entities"]
        .get(&t.entity)
        .ok_or_else(|| QueryError::UnknownEntity(t.entity.clone()))?;
    let mut ctx = Ctx {
        entity,
        params: vec![Param::Text(scope.tenant_id().to_string())],
    };

    // SELECT list: id and version, then declared fields (and an engine-managed state field).
    let mut names: Vec<String> = Vec::new();
    match &req.select {
        Some(sel) => {
            for f in sel {
                if f != "id" && !names.contains(f) {
                    ctx.column(f)?;
                    names.push(f.clone());
                }
            }
        }
        None => {
            if let Some(fields) = entity["fields"].as_object() {
                names.extend(fields.keys().cloned());
            }
            if let Some(sf) = entity["state"]["field"].as_str() {
                if !names.iter().any(|n| n == sf) {
                    names.push(sf.to_string());
                }
            }
        }
    }
    // Output columns get neutral aliases so `ORDER BY "col"` always means the table column,
    // never a text-cast output of the same name.
    let mut cols = vec![
        "\"id\"::text AS \"_c0\"".to_string(),
        "\"version\" AS \"_c1\"".to_string(),
    ];
    for (i, n) in names.iter().enumerate() {
        let alias = i + 2;
        cols.push(if ctx.field_type(n)? == "decimal" {
            format!("\"{n}\"::text AS \"_c{alias}\"")
        } else {
            format!("to_jsonb(\"{n}\") AS \"_c{alias}\"")
        });
    }

    let mut wheres = vec!["\"tenant_id\" = $1::text::uuid".to_string()];
    if entity["soft_delete"] != false {
        wheres.push("\"deleted_at\" IS NULL".to_string());
    }
    if let Some(f) = &req.filter {
        let mut nodes = 0;
        wheres.push(filter::compile(&mut ctx, f, 0, &mut nodes)?);
    }

    if let Some(text) = &req.search {
        let text = text.trim();
        if text.is_empty() || text.chars().count() > MAX_SEARCH_LEN {
            return Err(QueryError::BadSearch);
        }
        let mut fields: Vec<&String> = entity["fields"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, f)| f["searchable"] == true)
            .map(|(k, _)| k)
            .collect();
        if fields.is_empty() {
            return Err(QueryError::NotSearchable(t.entity.clone()));
        }
        fields.sort();
        // `%`, `_` and the escape character are literal in the user's text.
        let escaped = text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let n = ctx.bind(Param::Text(format!("%{escaped}%")));
        let any: Vec<String> = fields
            .iter()
            .map(|f| format!("\"{f}\" ILIKE ${n} ESCAPE '\\'"))
            .collect();
        wheres.push(format!("({})", any.join(" OR ")));
    }

    // Sorting: declared sort_keys only; id is always the final tiebreak.
    let sort_ok: Vec<&str> = entity["sort_keys"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for s in &req.sort {
        ctx.column(&s.field)?;
        if s.field != "id" && !sort_ok.contains(&s.field.as_str()) {
            return Err(QueryError::UnindexedSort(s.field.clone()));
        }
    }
    let mut order: Vec<(String, SortDir)> =
        req.sort.iter().map(|s| (s.field.clone(), s.dir)).collect();
    if !order.iter().any(|(f, _)| f == "id") {
        let dir = order.last().map(|o| o.1).unwrap_or(SortDir::Asc);
        order.push(("id".into(), dir));
    }

    // Keyset cursor, expanded lexicographically so each key keeps its own direction and nulls
    // sort last in both directions (`NULLS LAST`): the page after (c1..cn) is the OR over i of
    // "keys before i equal their cursor value, and key i comes after c_i". A null cursor value
    // has nothing after it, only ties to break on the next key.
    if let Some(after) = &req.after {
        if after.len() != order.len() {
            return Err(QueryError::BadCursor);
        }
        // (column, direction, bound placeholder with cast, or None for a null cursor value)
        let mut keys: Vec<(String, SortDir, Option<String>)> = Vec::new();
        for ((f, d), v) in order.iter().zip(after) {
            let col = ctx.column(f)?;
            if v.is_null() {
                if f == "id" {
                    return Err(QueryError::BadCursor);
                }
                keys.push((col, *d, None));
            } else {
                let (p, cast) = ctx.value(f, v)?;
                let n = ctx.bind(p);
                keys.push((col, *d, Some(format!("${n}{cast}"))));
            }
        }
        let mut alts = Vec::new();
        for i in 0..keys.len() {
            let mut parts: Vec<String> = keys[..i]
                .iter()
                .map(|(col, _, ph)| match ph {
                    Some(ph) => format!("{col} IS NOT DISTINCT FROM {ph}"),
                    None => format!("{col} IS NULL"),
                })
                .collect();
            let (col, dir, ph) = &keys[i];
            let Some(ph) = ph else { continue };
            let cmp = if *dir == SortDir::Asc { ">" } else { "<" };
            parts.push(format!("({col} {cmp} {ph} OR {col} IS NULL)"));
            alts.push(format!("({})", parts.join(" AND ")));
        }
        // Every key null (only the id can't be) leaves no later row: match nothing.
        wheres.push(if alts.is_empty() {
            "FALSE".to_string()
        } else {
            format!("({})", alts.join(" OR "))
        });
    }

    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let order_sql: Vec<String> = order
        .iter()
        .map(|(f, d)| {
            format!(
                "\"{f}\" {} NULLS LAST",
                if *d == SortDir::Asc { "ASC" } else { "DESC" }
            )
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM \"{}\".\"{}\" WHERE {} ORDER BY {} LIMIT {}",
        cols.join(", "),
        t.schema,
        t.entity,
        wheres.join(" AND "),
        order_sql.join(", "),
        limit + 1 // one extra row tells the caller whether another page exists
    );
    Ok(Planned {
        sql,
        columns: names,
        params: ctx.params,
    })
}
