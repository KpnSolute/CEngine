//! The migration loader (SOP sections 6 to 9): reads legacy rows table by table, parents first,
//! and brings each over with [`TypedPgStore::migration_import`]. The mapping is data (a plan the
//! customer's workspace owns), not code here. A row that cannot be brought over is recorded in
//! `migration_errors` by source table, source id and code, never by its contents, and the load
//! goes on. A second run resolves every row to the same target and writes nothing.
//!
//! Plan shape (JSON):
//! `{"source_system": "mjcc", "tables": [{"source": "vendors", "entity": "vendor",
//!   "fields": {"name": "name"}, "consts": {}, "transforms": {"label": {"format": "{year}-{month:02}"}},
//!   "refs": [{"target": "item", "source": "item_id", "table": "inventory_items"}],
//!   "ignore": ["note"], "key": "id", "source_key": "vendors", "actor": "created_by",
//!   "actor_const": "legacy", "at": "created_at", "at_const": 0}]}`

use std::collections::{BTreeMap, BTreeSet};

use ce_store::{StoreError, TenantScope};
use serde_json::{json, Map, Value};

use crate::import::ImportRecord;
use crate::typed::TypedPgStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Malformed(String),
    /// A source column the plan neither maps, references, transforms nor ignores.
    UnmappedColumn {
        table: String,
        column: String,
    },
}

/// What happened to the rows of one source table.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TableCount {
    pub written: u32,
    pub already_mapped: u32,
    pub rejected: u32,
}

pub type LoadReport = BTreeMap<String, TableCount>;

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn format_template(t: &str, row: &Map<String, Value>) -> Option<String> {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let j = rest[i..].find('}')? + i;
        let spec = &rest[i + 1..j];
        let (col, width) = match spec.split_once(':') {
            Some((c, w)) => (c, w.trim_start_matches('0').parse::<usize>().ok()),
            None => (spec, None),
        };
        let v = text(row.get(col)?)?;
        match width {
            Some(w) => out.push_str(&format!("{v:0>w$}")),
            None => out.push_str(&v),
        }
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

fn transform(spec: &Value, row: &Map<String, Value>) -> Option<Value> {
    if let Some(f) = spec["format"].as_str() {
        return format_template(f, row).map(Value::String);
    }
    let cols = spec["month_start"]
        .as_array()
        .or(spec["month_end"].as_array())?;
    let (y, m) = (
        row.get(cols.first()?.as_str()?)?.as_i64()?,
        row.get(cols.get(1)?.as_str()?)?.as_i64()?,
    );
    if !(1..=12).contains(&m) {
        return None;
    }
    let d = if spec.get("month_end").is_some() {
        days_in_month(y, m)
    } else {
        1
    };
    Some(json!(format!("{y:04}-{m:02}-{d:02}")))
}

/// `{"table": "vendors", "by": "vendor_id", "field": "name"}`: the value of `field` on the row of
/// `table` whose key (default `id`) equals this row's `by` column.
fn lookup_value(
    spec: &Value,
    row: &Map<String, Value>,
    tables: &Map<String, Value>,
) -> Option<Value> {
    let want = row.get(spec["by"].as_str()?).and_then(text)?;
    let key = spec["key"].as_str().unwrap_or("id");
    tables
        .get(spec["table"].as_str()?)?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|p| p.get(key).and_then(text).as_deref() == Some(want.as_str()))
        .and_then(|p| p.get(spec["field"].as_str()?))
        .filter(|v| !v.is_null())
        .cloned()
}

/// A `by` entry is a column name, or `{"col": "month", "add": 1}` for a source that counts
/// differently from the parent (the legacy price queue counts months from 0).
fn by_value(spec: &Value, row: &Map<String, Value>) -> Option<String> {
    match spec.as_str() {
        Some(c) => row.get(c).and_then(text),
        None => {
            let n = row.get(spec["col"].as_str()?)?.as_i64()?;
            Some((n + spec["add"].as_i64().unwrap_or(0)).to_string())
        }
    }
}

/// `{"table": "month_periods", "match": {"month": {"col": "month", "add": 1}, "year": "year"},
/// "field": "id"}`: `field` of the row of `table` whose columns equal these values of this row.
fn find_value(
    spec: &Value,
    row: &Map<String, Value>,
    tables: &Map<String, Value>,
) -> Option<Value> {
    let want = spec["match"].as_object()?;
    tables
        .get(spec["table"].as_str()?)?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|p| {
            want.iter().all(|(pc, rc)| {
                by_value(rc, row).is_some() && by_value(rc, row) == p.get(pc).and_then(text)
            })
        })
        .and_then(|p| p.get(spec["field"].as_str()?))
        .cloned()
}

fn used_columns(t: &Value) -> BTreeSet<String> {
    let mut used = BTreeSet::new();
    fn add(used: &mut BTreeSet<String>, v: &Value) {
        if let Some(s) = v.as_str() {
            used.insert(s.to_string());
        }
    }
    used.insert(t["key"].as_str().unwrap_or("id").to_string());
    for v in t["fields"].as_object().into_iter().flat_map(|o| o.values()) {
        add(&mut used, v);
    }
    for v in t["ignore"].as_array().into_iter().flatten() {
        add(&mut used, v);
    }
    for k in ["actor", "at", "state_from"] {
        add(&mut used, &t[k]);
    }
    for d in t["derive"].as_object().into_iter().flat_map(|o| o.values()) {
        for v in d["find"]["match"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
        {
            add(&mut used, v);
            add(&mut used, &v["col"]);
        }
    }
    for r in t["refs"].as_array().into_iter().flatten() {
        add(&mut used, &r["source"]);
        for v in r["by"].as_object().into_iter().flat_map(|o| o.values()) {
            add(&mut used, v);
            add(&mut used, &v["col"]);
        }
    }
    for spec in t["transforms"]
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
    {
        if let Some(f) = spec["format"].as_str() {
            let mut rest = f;
            while let Some(i) = rest.find('{') {
                if let Some(j) = rest[i..].find('}') {
                    let name = rest[i + 1..i + j].split(':').next().unwrap_or("");
                    used.insert(name.to_string());
                    rest = &rest[i + j + 1..];
                } else {
                    break;
                }
            }
        }
        for k in ["month_start", "month_end"] {
            for c in spec[k].as_array().into_iter().flatten() {
                add(&mut used, c);
            }
        }
        add(&mut used, &spec["lookup"]["by"]);
        add(&mut used, &spec["map"]["from"]);
        add(&mut used, &spec["ordinal"]["within"]);
    }
    used
}

/// Checks the plan against the rows before anything is written: every column a row carries must
/// be accounted for, so a column cannot be dropped without the plan saying so.
pub fn check_plan(plan: &Value, tables: &Map<String, Value>) -> Result<(), PlanError> {
    let list = plan["tables"]
        .as_array()
        .ok_or_else(|| PlanError::Malformed("tables must be a list".into()))?;
    for t in list {
        let source = t["source"]
            .as_str()
            .ok_or_else(|| PlanError::Malformed("table without source".into()))?;
        if t["entity"].as_str().is_none() {
            return Err(PlanError::Malformed(format!("{source}: no entity")));
        }
        let used = used_columns(t);
        for row in tables
            .get(source)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for col in row.as_object().into_iter().flat_map(|o| o.keys()) {
                if !used.contains(col) {
                    return Err(PlanError::UnmappedColumn {
                        table: source.into(),
                        column: col.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn code_of(e: &StoreError) -> &'static str {
    match e {
        StoreError::Constraint(_) => "CONSTRAINT",
        StoreError::BadData(_) => "BAD_DATA",
        _ => "STORE_ERROR",
    }
}

impl TypedPgStore {
    /// Loads `tables` (source table name to a list of row objects) as `plan` says, in plan order.
    pub fn migration_load(
        &self,
        scope: &TenantScope,
        run: &str,
        plan: &Value,
        tables: &Map<String, Value>,
        default_actor: &str,
        default_at: i64,
    ) -> Result<LoadReport, PlanError> {
        check_plan(plan, tables)?;
        let system = plan["source_system"]
            .as_str()
            .unwrap_or("legacy")
            .to_string();
        let tenant = scope.tenant_id().to_string();
        let mut report = LoadReport::new();

        for t in plan["tables"].as_array().into_iter().flatten() {
            let source = t["source"].as_str().unwrap_or_default();
            let entity = t["entity"].as_str().unwrap_or_default();
            let source_key = t["source_key"].as_str().unwrap_or(source);
            let key = t["key"].as_str().unwrap_or("id");
            let count = report.entry(source_key.to_string()).or_default();
            let mut seen_in_group: BTreeMap<String, i64> = BTreeMap::new();
            for row in tables
                .get(source)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(row) = row.as_object() else { continue };
                let Some(sid) = row.get(key).and_then(text) else {
                    let _ = self.migration_reject(
                        &tenant,
                        run,
                        source_key,
                        "?",
                        "NO_KEY",
                        "row has no key",
                        None,
                    );
                    count.rejected += 1;
                    continue;
                };
                let payload = serde_json::to_vec(row).unwrap_or_default();
                let reject = |code: &str, msg: &str| {
                    let _ = self.migration_reject(
                        &tenant,
                        run,
                        source_key,
                        &sid,
                        code,
                        msg,
                        Some(&payload),
                    );
                };

                // Columns the source row does not have but the plan can find in another table
                // (the price queue names a month and a year; the parent is keyed by period).
                let derived;
                let row = if let Some(d) = t["derive"].as_object() {
                    let mut r = row.clone();
                    for (name, spec) in d {
                        if let Some(v) = find_value(&spec["find"], &r, tables) {
                            r.insert(name.clone(), v);
                        }
                    }
                    derived = r;
                    &derived
                } else {
                    row
                };
                let mut data = Map::new();
                for (target, col) in t["fields"].as_object().into_iter().flatten() {
                    if let Some(v) = col.as_str().and_then(|c| row.get(c)) {
                        if !v.is_null() {
                            data.insert(target.clone(), v.clone());
                        }
                    }
                }
                for (target, v) in t["consts"].as_object().into_iter().flatten() {
                    data.insert(target.clone(), v.clone());
                }
                let mut bad = None;
                for (target, spec) in t["transforms"].as_object().into_iter().flatten() {
                    let found = if let Some(m) = spec.get("map") {
                        // One column's value looked up in a fixed table; an unknown value is refused.
                        m["from"]
                            .as_str()
                            .and_then(|c| row.get(c))
                            .and_then(text)
                            .and_then(|v| m["values"].get(&v).cloned())
                    } else if spec.get("lookup").is_some() {
                        lookup_value(&spec["lookup"], row, tables)
                    } else if let Some(col) = spec["ordinal"]["within"].as_str() {
                        // 1, 2, 3 ... in the order the rows appear, restarting per parent.
                        let n = seen_in_group
                            .entry(row.get(col).and_then(text).unwrap_or_default())
                            .or_insert(0);
                        *n += 1;
                        Some(json!(*n))
                    } else {
                        transform(spec, row)
                    };
                    match found {
                        Some(v) => {
                            data.insert(target.clone(), v);
                        }
                        None => bad = Some(("BAD_VALUE", "a derived field could not be computed")),
                    }
                }
                let mut parent_sid = None;
                for r in t["refs"].as_array().into_iter().flatten() {
                    let (target, ptable) = (
                        r["target"].as_str().unwrap_or_default(),
                        r["table"].as_str().unwrap_or_default(),
                    );
                    let pkey = r["table_key"].as_str().unwrap_or(ptable);
                    // A reference is a single column, or a set of columns that find one parent row.
                    let psid = if let Some(col) = r["source"].as_str() {
                        row.get(col).and_then(text)
                    } else if let Some(by) = r["by"].as_object() {
                        let parents = tables.get(ptable).and_then(Value::as_array);
                        let pk = r["parent_key"].as_str().unwrap_or("id");
                        parents
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_object)
                            .find(|p| {
                                by.iter()
                                    .all(|(pc, rc)| by_value(rc, row) == p.get(pc).and_then(text))
                            })
                            .and_then(|p| p.get(pk).and_then(text))
                    } else {
                        None
                    };
                    let target_id = psid.as_deref().and_then(|p| {
                        self.migration_lookup(&tenant, &system, pkey, p)
                            .ok()
                            .flatten()
                    });
                    match target_id {
                        Some(id) => {
                            data.insert(target.to_string(), json!(id));
                            parent_sid = parent_sid.or(psid);
                        }
                        None => {
                            bad =
                                Some(("MISSING_PARENT", "the referenced parent row was not loaded"))
                        }
                    }
                }
                if let Some((code, msg)) = bad {
                    reject(code, msg);
                    count.rejected += 1;
                    continue;
                }

                let actor = t["actor"]
                    .as_str()
                    .and_then(|c| row.get(c))
                    .and_then(text)
                    .or_else(|| t["actor_const"].as_str().map(String::from))
                    .unwrap_or_else(|| default_actor.to_string());
                let at = t["at"]
                    .as_str()
                    .and_then(|c| row.get(c))
                    .and_then(Value::as_i64)
                    .or_else(|| t["at_const"].as_i64())
                    .unwrap_or(default_at);
                let state = t["state_from"]
                    .as_str()
                    .and_then(|c| row.get(c))
                    .and_then(Value::as_str);
                let rec = ImportRecord {
                    run,
                    source_system: &system,
                    source_table: source_key,
                    source_id: &sid,
                    source_parent_id: parent_sid.as_deref(),
                    entity,
                    data: Value::Object(data),
                    actor: &actor,
                    at,
                    state,
                };
                match self.migration_import(scope, &rec) {
                    Ok(o) if o.created => count.written += 1,
                    Ok(_) => count.already_mapped += 1,
                    Err(e) => {
                        reject(code_of(&e), "the target refused the row");
                        count.rejected += 1;
                    }
                }
            }
        }
        Ok(report)
    }
}
