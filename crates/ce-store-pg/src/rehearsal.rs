//! The migration driver: one call plans a run, loads the legacy tables the plan describes,
//! lets the engine recompute derived fields, runs the reconciliation checks, and closes the run.
//! The mapping (plan) and the checks are data the customer's workspace owns; nothing here knows
//! a legacy table by name. Checks compare a figure read from the legacy rows with a figure the
//! target gives back, to the cent, with no tolerance.
//!
//! Checks file (JSON array):
//! `{"key": "invoice_total", "scope_type": "supplier_invoice", "scope_id": "all",
//!   "legacy": {"table": "invoices", "op": "sum", "column": "stated_total"},
//!   "target": "SELECT coalesce(sum(total), 0)::text FROM {schema}.supplier_invoice",
//!   "explanation": null}`
//! `op` is `sum` or `count` (`count` ignores `column`); `where` optionally filters legacy rows by
//! equality. `target` must be one SELECT; it runs in a read-only transaction.

use crate::loader::LoadReport;
use crate::provenance::RunSpec;
use crate::{Tls, TypedPgStore};
use ce_store::{StoreError, TenantScope};
use serde_json::{Map, Value};

pub struct Params<'a> {
    pub url: &'a str,
    pub tls: &'a Tls,
    pub tenant_id: &'a str,
    pub installation_id: &'a str,
    pub source_system: &'a str,
    /// A name for the snapshot (not a credential), kept on the run record.
    pub source_ref: &'a str,
    pub migration_version: &'a str,
    pub initiated_by: &'a str,
    pub default_actor: &'a str,
    pub now_unix: i64,
}

#[derive(Debug)]
pub struct CheckResult {
    pub key: String,
    pub scope_id: String,
    pub legacy: String,
    pub target: String,
    pub delta: String,
    pub pass: bool,
}

#[derive(Debug)]
pub struct Rehearsal {
    pub run: String,
    pub status: String,
    pub load: LoadReport,
    pub checks: Vec<CheckResult>,
}

fn bad(m: &'static str) -> StoreError {
    StoreError::BadData(m)
}

fn cell(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The legacy side of a check, computed by the database from the exported values so decimals
/// are exact.
fn legacy_figure(
    c: &mut crate::Client,
    tables: &Map<String, Value>,
    spec: &Value,
) -> Result<String, StoreError> {
    let table = spec["table"].as_str().ok_or(bad("legacy.table missing"))?;
    let rows = tables
        .get(table)
        .and_then(Value::as_array)
        .ok_or(bad("legacy table not in the export"))?;
    let filter = spec["where"].as_object();
    let keep = |r: &Value| {
        filter.is_none_or(|f| f.iter().all(|(k, v)| r.get(k).and_then(cell) == cell(v)))
    };
    match spec["op"].as_str() {
        Some("count") => Ok(rows.iter().filter(|r| keep(r)).count().to_string()),
        Some("sum") => {
            let col = spec["column"]
                .as_str()
                .ok_or(bad("legacy.column missing"))?;
            let vals: Vec<String> = rows
                .iter()
                .filter(|r| keep(r))
                .filter_map(|r| r.get(col).and_then(cell))
                .collect();
            let row = c
                .query_one(
                    "SELECT coalesce(sum(v::numeric), 0)::text FROM unnest($1::text[]) AS v",
                    &[&vals],
                )
                .map_err(|_| bad("legacy value is not a number"))?;
            Ok(row.get(0))
        }
        _ => Err(bad("legacy.op must be sum or count")),
    }
}

fn target_figure(c: &mut crate::Client, schema: &str, sql: &str) -> Result<String, StoreError> {
    let t = sql.trim();
    if !t.to_ascii_lowercase().starts_with("select") || t.contains(';') {
        return Err(bad("a target check must be one SELECT"));
    }
    let q = t.replace("{schema}", &format!("\"{schema}\""));
    c.batch_execute("BEGIN READ ONLY")
        .map_err(|_| bad("cannot open a read-only transaction"))?;
    let r = c.query_one(q.as_str(), &[]);
    let _ = c.batch_execute("ROLLBACK");
    r.map(|row| row.get::<_, String>(0))
        .map_err(|_| bad("target check failed"))
}

#[allow(clippy::too_many_arguments)]
/// Runs the whole migration against `store`'s schema. The run ends `passed` only if every
/// check has a zero delta (or a committed explanation) and no row was rejected.
pub fn run(
    store: &TypedPgStore,
    scope: &TenantScope,
    schema: &str,
    p: &Params<'_>,
    plan: &Value,
    tables: &Map<String, Value>,
    checks: &Value,
    settle: &dyn Fn() -> Result<(), StoreError>,
) -> Result<Rehearsal, StoreError> {
    let list = checks.as_array().ok_or(bad("checks must be an array"))?;
    let run = store.migration_start_run(&RunSpec {
        tenant_id: p.tenant_id,
        installation_id: p.installation_id,
        source_system: p.source_system,
        source_project_ref: p.source_ref,
        migration_version: p.migration_version,
        initiated_by: p.initiated_by,
    })?;
    store.migration_set_run_status(&run, "running")?;
    let load = store
        .migration_load(scope, &run, plan, tables, p.default_actor, p.now_unix)
        .map_err(|_| bad("the plan does not fit the export"))?;
    // Derived fields are recomputed by the engine, not copied from the legacy rows.
    settle()?;
    store.migration_set_run_status(&run, "reconciling")?;

    let mut c = crate::connect(p.url, p.tls)?;
    let mut results = Vec::new();
    for ck in list {
        let key = ck["key"].as_str().ok_or(bad("check.key missing"))?;
        let scope_type = ck["scope_type"].as_str().unwrap_or("run");
        let scope_id = ck["scope_id"].as_str().unwrap_or("all");
        let legacy = legacy_figure(&mut c, tables, &ck["legacy"])?;
        let target = target_figure(
            &mut c,
            schema,
            ck["target"].as_str().ok_or(bad("check.target missing"))?,
        )?;
        let o = store.migration_check(
            p.tenant_id,
            &run,
            key,
            scope_type,
            scope_id,
            &legacy,
            &target,
            ck["explanation"].as_str(),
        )?;
        results.push(CheckResult {
            key: key.into(),
            scope_id: scope_id.into(),
            legacy,
            target,
            delta: o.delta,
            pass: o.pass,
        });
    }
    let source_count: i64 = tables
        .values()
        .filter_map(Value::as_array)
        .map(|a| a.len() as i64)
        .sum();
    let status = store.migration_finish_run(&run, source_count)?;
    Ok(Rehearsal {
        run,
        status,
        load,
        checks: results,
    })
}
