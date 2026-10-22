//! Reconciliation compares legacy and target totals to the cent: no tolerance, an explanation
//! only by a committed note, and a run passes only when every check does.
use ce_manifest::validate;
use ce_store_pg::provenance::RunSpec;
use ce_store_pg::{connect, Tls, TypedPgStore};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

const T: &str = "11111111-1111-1111-1111-111111111111";

fn url() -> Option<String> {
    match std::env::var("CE_TEST_DATABASE_URL") {
        Ok(u) => Some(u),
        Err(_) => {
            assert!(
                std::env::var("CE_REQUIRE_DB").is_err(),
                "CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not"
            );
            eprintln!("skipping: CE_TEST_DATABASE_URL not set");
            None
        }
    }
}

static N: AtomicU32 = AtomicU32::new(0);

fn manifest() -> ce_manifest::Manifest {
    validate(
        json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"doc": {"fields": {"title": {"type": "string", "required": true}}}},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

/// An installed schema that is dropped again when the test ends.
struct Env {
    store: Option<TypedPgStore>,
    url: String,
    schema: String,
}

impl Env {
    fn new(url: &str) -> Env {
        Env::with(url, &manifest())
    }
    fn with(url: &str, m: &ce_manifest::Manifest) -> Env {
        let schema = format!(
            "ce_mg_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(url, m, &schema).unwrap();
        Env {
            store: Some(store),
            url: url.into(),
            schema,
        }
    }
    fn s(&self) -> &TypedPgStore {
        self.store.as_ref().unwrap()
    }
    fn run(&self, by: &str) -> String {
        self.s()
            .migration_start_run(&RunSpec {
                tenant_id: T,
                installation_id: "inst",
                source_system: "mjcc",
                source_project_ref: "synthetic",
                migration_version: "1",
                initiated_by: by,
            })
            .unwrap()
    }
    fn count(&self, table: &str) -> i64 {
        let mut c = connect(&self.url, &Tls::Off).unwrap();
        c.query_one(
            &format!(r#"SELECT count(*) FROM "{}"."{table}""#, self.schema),
            &[],
        )
        .unwrap()
        .get(0)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let (store, url, schema) = (self.store.take(), self.url.clone(), self.schema.clone());
        let _ = std::thread::spawn(move || {
            drop(store);
            if let Ok(mut c) = connect(&url, &Tls::Off) {
                let _ = c.batch_execute(&format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#));
            }
        })
        .join();
    }
}

#[test]
fn a_check_passes_only_on_a_zero_delta() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let r = e.run("t");
    let ok = e
        .s()
        .migration_check(
            T,
            &r,
            "valuation_total",
            "period",
            "2026-09",
            "1234.5600",
            "1234.56",
            None,
        )
        .unwrap();
    assert!(ok.pass, "{ok:?}");
    let off = e
        .s()
        .migration_check(
            T,
            &r,
            "valuation_total",
            "period",
            "2026-10",
            "1234.56",
            "1234.55",
            None,
        )
        .unwrap();
    assert!(!off.pass, "a one-cent difference is not close enough");
    assert_eq!(off.delta, "0.01");
    let neg = e
        .s()
        .migration_check(
            T,
            &r,
            "receipts_total",
            "period",
            "2026-10",
            "10.00",
            "10.10",
            None,
        )
        .unwrap();
    assert!(!neg.pass);
    assert_eq!(neg.delta, "-0.10");
}

#[test]
fn a_delta_passes_only_with_a_committed_explanation() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let r = e.run("t");
    let c = e
        .s()
        .migration_check(
            T,
            &r,
            "rounding",
            "period",
            "2026-10",
            "5.005",
            "5.01",
            Some("legacy rounded half-up per item; see docs/RECONCILIATION.md"),
        )
        .unwrap();
    assert!(c.pass);
    assert!(e
        .s()
        .migration_check(T, &r, "x", "p", "", "1", "1", Some(""))
        .is_err());
    assert!(e
        .s()
        .migration_check(T, &r, "x", "p", "", "1", "1", Some(&"x".repeat(201)))
        .is_err());
}

#[test]
fn a_run_passes_only_when_every_check_passes_and_no_row_was_rejected() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let status = |run: &str| -> String {
        let mut c = connect(&e.url, &Tls::Off).unwrap();
        c.query_one(
            &format!(
                r#"SELECT status FROM "{}".migration_runs WHERE id = $1::text::uuid"#,
                e.schema
            ),
            &[&run],
        )
        .unwrap()
        .get(0)
    };

    // All checks pass.
    let r = e.run("t");
    e.s().migration_set_run_status(&r, "running").unwrap();
    e.s()
        .migration_check(T, &r, "k", "period", "a", "2.00", "2.00", None)
        .unwrap();
    e.s().migration_set_run_status(&r, "reconciling").unwrap();
    assert_eq!(e.s().migration_finish_run(&r, 1).unwrap(), "passed");
    assert_eq!(status(&r), "passed");

    // One failed check.
    let r = e.run("t");
    e.s().migration_set_run_status(&r, "running").unwrap();
    e.s()
        .migration_check(T, &r, "k", "period", "a", "2.00", "2.00", None)
        .unwrap();
    e.s()
        .migration_check(T, &r, "k", "period", "b", "2.00", "1.99", None)
        .unwrap();
    assert_eq!(e.s().migration_finish_run(&r, 1).unwrap(), "failed");

    // A rejected row, all checks passing.
    let r = e.run("t");
    e.s().migration_set_run_status(&r, "running").unwrap();
    e.s()
        .migration_check(T, &r, "k", "period", "a", "2.00", "2.00", None)
        .unwrap();
    e.s()
        .migration_reject(
            T,
            &r,
            "invoice_items",
            "9",
            "NEGATIVE_QTY",
            "quantity below zero",
            None,
        )
        .unwrap();
    assert_eq!(e.s().migration_finish_run(&r, 2).unwrap(), "failed");

    // No checks at all is not a pass.
    let r = e.run("t");
    e.s().migration_set_run_status(&r, "running").unwrap();
    assert_eq!(e.s().migration_finish_run(&r, 0).unwrap(), "failed");

    // A planned run cannot be finished, and a finished one cannot be re-opened.
    let r = e.run("t");
    assert!(e.s().migration_finish_run(&r, 0).is_err());
    assert!(e.s().migration_set_run_status(&r, "reconciling").is_err());
}

#[test]
fn a_rejected_row_keeps_a_hash_and_a_code_never_the_payload() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let r = e.run("t");
    let payload = br#"{"name":"CUSTOMER SECRET NAME","qty":-3}"#;
    e.s()
        .migration_reject(
            T,
            &r,
            "inventory_items",
            "12",
            "NEGATIVE_QTY",
            "quantity below zero",
            Some(payload),
        )
        .unwrap();
    assert!(e
        .s()
        .migration_reject(T, &r, "t", "1", "C", &"m".repeat(201), None)
        .is_err());
    assert!(e
        .s()
        .migration_reject(T, &r, "t", "1", "", "m", None)
        .is_err());
    assert_eq!(
        e.count("migration_errors"),
        1,
        "the invalid rejections wrote nothing"
    );
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let row = c
        .query_one(
            &format!(
                r#"SELECT row_to_json(m)::text FROM "{}".migration_errors m"#,
                e.schema
            ),
            &[],
        )
        .unwrap();
    let text: String = row.get(0);
    assert!(
        !text.contains("SECRET"),
        "payload text must not be stored: {text}"
    );
    assert!(text.contains("NEGATIVE_QTY") && text.contains("inventory_items"));
    let hash = text
        .split("\"payload_hash\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(hash.len(), 64);
}

// ---------------------------------------------------------------------------------------------
// The loader against the real inventory module, and reconciliation of what it loaded.
// ---------------------------------------------------------------------------------------------

fn inventory_manifest() -> ce_manifest::Manifest {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0", "title": "Kitchen"},
        "entities": {
            "period": {"fields": {
                "label": {"type": "string", "required": true, "unique": true, "max_length": 80, "indexed": true},
                "starts_on": {"type": "date", "required": true},
                "ends_on": {"type": "date", "required": true},
                "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true},
                "week_1_locked": {"type": "boolean", "default": false},
                "week_2_locked": {"type": "boolean", "default": false},
                "week_3_locked": {"type": "boolean", "default": false},
                "week_4_locked": {"type": "boolean", "default": false}
            }, "sort_keys": ["label"]},
            "inventory_category": {"fields": {"name": {"type": "string", "required": true, "unique": true, "max_length": 80}}},
            "vendor": {"fields": {"name": {"type": "string", "required": true, "unique": true, "max_length": 120}}},
            "stock_item_detail": {"fields": {
                "item": {"type": "ref", "ref": "stock_item", "required": true},
                "category": {"type": "ref", "ref": "inventory_category"}
            }}
        },
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let with_audit = ce_tools::compose(&shell, &["audit"]).unwrap();
    let plugin = ce_plugin::parse(
        include_str!("../../ce-plugin/tests/fixtures/mjcc-legacy-valuation/plugin.toml"),
        &ce_plugin::builtin_registry(),
        &semver::Version::new(0, 2, 0),
    )
    .unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    let c = ce_plugin::compose(
        &with_audit,
        "business.inventory",
        Some((&plugin, digest.as_str())),
    )
    .unwrap();
    ce_manifest::validate_composed(c.manifest.to_string().as_bytes()).unwrap()
}

fn read_json(rel: &str) -> serde_json::Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../")
        .join(rel);
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn scope() -> ce_store::TenantScope {
    ce_auth::Verifier::new(b"0123456789abcdef0123456789abcdef")
        .unwrap()
        .public_scope(T)
        .unwrap()
}

fn load(e: &Env, run: &str, tables: &serde_json::Value) -> ce_store_pg::loader::LoadReport {
    let plan = read_json("tests/migration/plans/mjcc_inventory_v1.json");
    let report = e
        .s()
        .migration_load(
            &scope(),
            run,
            &plan,
            tables.as_object().unwrap(),
            "legacy",
            1_700_000_000,
        )
        .unwrap();
    // Derived fields (line totals, ending values) are recomputed by the engine, not copied.
    ce_server::settle_imported(e.s(), &inventory_manifest(), &scope(), 1_700_000_000).unwrap();
    report
}

#[test]
fn the_fixture_loads_parents_then_children_and_nothing_is_dropped() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let report = load(&e, &e.run("rehearsal"), &fx["tables"]);
    for (table, n) in [
        ("inventory_categories", 2),
        ("vendors", 1),
        ("month_periods", 1),
        ("inventory_items", 2),
        ("inventory_items#category", 2),
        ("monthly_inventory", 2),
        ("inventory_transactions", 3),
        ("invoices", 2),
        ("invoice_items", 3),
    ] {
        let c = &report[table];
        assert_eq!((c.written, c.rejected), (n, 0), "{table}: {c:?}");
    }
    assert_eq!(e.count("stock_movement"), 3);
    assert_eq!(e.count("migration_errors"), 0);
}

#[test]
fn a_second_load_creates_nothing_and_the_targets_are_the_same() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    load(&e, &e.run("first"), &fx["tables"]);
    let ids = |e: &Env| -> Vec<String> {
        let mut c = connect(&e.url, &Tls::Off).unwrap();
        c.query(&format!(r#"SELECT source_table || ':' || source_id || '=' || target_id::text FROM "{}".migration_id_map ORDER BY 1"#, e.schema), &[])
            .unwrap().iter().map(|r| r.get(0)).collect()
    };
    let (before, rows) = (
        ids(&e),
        (
            e.count("stock_line"),
            e.count("stock_movement"),
            e.count("audit_event"),
        ),
    );
    let again = load(&e, &e.run("second"), &fx["tables"]);
    assert!(
        again.values().all(|c| c.written == 0 && c.rejected == 0),
        "{again:?}"
    );
    assert_eq!(ids(&e), before, "same target ids");
    assert_eq!(
        (
            e.count("stock_line"),
            e.count("stock_movement"),
            e.count("audit_event")
        ),
        rows
    );
}

#[test]
fn a_row_with_a_missing_parent_or_an_unmapped_column_is_refused_by_name() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let mut fx = read_json("tests/migration/fixtures/normal_month.json");
    // A transaction for an item that does not exist.
    fx["tables"]["inventory_transactions"].as_array_mut().unwrap().push(
        json!({"id": "t-orphan", "item_id": "nope", "period_id": "p1", "week": 1, "received": "1", "pulled": "0"}),
    );
    let run = e.run("t");
    let report = load(&e, &run, &fx["tables"]);
    assert_eq!(report["inventory_transactions"].rejected, 1);
    assert_eq!(report["inventory_transactions"].written, 3);
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let row = c.query_one(&format!(r#"SELECT source_table, source_id, error_code, payload_hash FROM "{}".migration_errors"#, e.schema), &[]).unwrap();
    assert_eq!(
        (
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2)
        ),
        (
            "inventory_transactions".into(),
            "t-orphan".into(),
            "MISSING_PARENT".into()
        )
    );
    assert_eq!(row.get::<_, String>(3).len(), 64);

    // A column nobody mapped stops the whole load before a row is written.
    let mut fx2 = read_json("tests/migration/fixtures/normal_month.json");
    fx2["tables"]["vendors"][0]["secret_note"] = json!("x");
    let e2 = Env::with(&url, &inventory_manifest());
    let plan = read_json("tests/migration/plans/mjcc_inventory_v1.json");
    let err = e2
        .s()
        .migration_load(
            &scope(),
            &e2.run("t"),
            &plan,
            fx2["tables"].as_object().unwrap(),
            "legacy",
            0,
        )
        .unwrap_err();
    assert!(
        matches!(err, ce_store_pg::loader::PlanError::UnmappedColumn { ref column, .. } if column == "secret_note")
    );
    assert_eq!(e2.count("vendor"), 0);
}

/// Legacy figures recomputed with the legacy formulas (backend/inventory_formulas.py) in
/// Postgres numeric, independent of the engine's computed fields.
fn legacy_totals(e: &Env, fx: &serde_json::Value) -> (String, String, String, String) {
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let t = &fx["tables"];
    let (mut recv, mut pull, mut end_qty, mut end_val) = (
        "0".to_string(),
        "0".to_string(),
        "0".to_string(),
        "0".to_string(),
    );
    for l in t["monthly_inventory"].as_array().unwrap() {
        let (item, period) = (&l["item_id"], &l["period_id"]);
        let moves: Vec<_> = t["inventory_transactions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| &m["item_id"] == item && &m["period_id"] == period)
            .collect();
        let mut sum = |k: &str| -> String {
            moves
                .iter()
                .map(|m| m[k].as_str().unwrap_or("0").to_string())
                .fold("0".to_string(), |a, b| {
                    c.query_one(
                        "SELECT ($1::text::numeric + $2::text::numeric)::text",
                        &[&a, &b],
                    )
                    .unwrap()
                    .get(0)
                })
        };
        let (r, p) = (sum("received"), sum("pulled"));
        let (open, ouc, price) = (
            l["opening_oh"].as_str().unwrap(),
            l["opening_unit_cost"].as_str().unwrap(),
            l["unit_price"].as_str().unwrap(),
        );
        let row = c.query_one(
            "WITH x AS (SELECT $1::text::numeric o, $2::text::numeric ouc, $3::text::numeric pr, $4::text::numeric r, $5::text::numeric p)
             SELECT r::text, p::text, greatest(0, o + r - p)::text,
                    CASE WHEN greatest(0, o + r - p) <= 0 THEN '0' ELSE round(greatest(0, o*ouc + r*pr - p*pr), 2)::text END FROM x",
            &[&open, &ouc, &price, &r, &p]).unwrap();
        let add = |c: &mut ce_store_pg::Client, a: &str, b: String| -> String {
            c.query_one(
                "SELECT ($1::text::numeric + $2::text::numeric)::text",
                &[&a, &b],
            )
            .unwrap()
            .get(0)
        };
        recv = add(&mut c, &recv, row.get(0));
        pull = add(&mut c, &pull, row.get(1));
        end_qty = add(&mut c, &end_qty, row.get(2));
        end_val = add(&mut c, &end_val, row.get(3));
    }
    (recv, pull, end_qty, end_val)
}

#[test]
fn engine_totals_match_the_legacy_formulas_to_the_cent() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    e.s().migration_set_run_status(&run, "reconciling").unwrap();

    let (l_recv, l_pull, l_qty, l_val) = legacy_totals(&e, &fx);
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let t = c
        .query_one(
            &format!(
                r#"SELECT coalesce(sum(received),0)::text, coalesce(sum(pulled),0)::text,
                           coalesce(sum(ending_qty),0)::text, coalesce(sum(ending_value),0)::text
                    FROM "{}".stock_line"#,
                e.schema
            ),
            &[],
        )
        .unwrap();
    let (t_recv, t_pull, t_qty, t_val): (String, String, String, String) =
        (t.get(0), t.get(1), t.get(2), t.get(3));

    let mut passes = 0;
    for (key, legacy, target) in [
        ("received_total", l_recv, t_recv),
        ("pulled_total", l_pull, t_pull),
        ("ending_qty_total", l_qty, t_qty),
        ("ending_value_total", l_val, t_val),
    ] {
        let o = e
            .s()
            .migration_check(T, &run, key, "period", "2026-11", &legacy, &target, None)
            .unwrap();
        assert!(
            o.pass,
            "{key}: legacy {legacy} target {target} delta {}",
            o.delta
        );
        passes += 1;
    }
    // Counts reconcile too: every source row has exactly one target.
    for (key, table, entity) in [
        ("items_count", "inventory_items", "stock_item"),
        ("lines_count", "monthly_inventory", "stock_line"),
        (
            "movements_count",
            "inventory_transactions",
            "stock_movement",
        ),
    ] {
        let legacy = fx["tables"][table].as_array().unwrap().len().to_string();
        let target = e.count(entity).to_string();
        assert!(
            e.s()
                .migration_check(T, &run, key, "table", table, &legacy, &target, None)
                .unwrap()
                .pass,
            "{key}"
        );
        passes += 1;
    }
    assert_eq!(passes, 7);
    assert_eq!(e.s().migration_finish_run(&run, 7).unwrap(), "passed");
}

#[test]
fn a_target_that_differs_by_a_cent_fails_the_run() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    // Damage the loaded data: one line's stored ending value is a cent off.
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    c.batch_execute(&format!(
        r#"UPDATE "{}".stock_line SET ending_value = ending_value + 0.01"#,
        e.schema
    ))
    .unwrap();
    let (_, _, _, l_val) = legacy_totals(&e, &fx);
    let t_val: String = c
        .query_one(
            &format!(
                r#"SELECT coalesce(sum(ending_value),0)::text FROM "{}".stock_line"#,
                e.schema
            ),
            &[],
        )
        .unwrap()
        .get(0);
    let o = e
        .s()
        .migration_check(
            T,
            &run,
            "ending_value_total",
            "period",
            "2026-11",
            &l_val,
            &t_val,
            None,
        )
        .unwrap();
    assert!(!o.pass, "legacy {l_val} target {t_val}");
    e.s().migration_set_run_status(&run, "reconciling").unwrap();
    assert_eq!(e.s().migration_finish_run(&run, 1).unwrap(), "failed");
}

#[test]
fn invoices_reconcile_per_invoice_and_line_totals_match_to_the_cent() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    e.s().migration_set_run_status(&run, "reconciling").unwrap();
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let t = &fx["tables"];
    let num = |c: &mut ce_store_pg::Client,
               sql: &str,
               args: &[&(dyn ce_store_pg::ToSql + Sync)]|
     -> String { c.query_one(sql, args).unwrap().get(0) };
    let mut passes = 0;
    for inv in t["invoices"].as_array().unwrap() {
        let id = inv["id"].as_str().unwrap();
        // Legacy side: the stated figures and the lines' own quantity x price.
        let goods = t["invoice_items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["invoice_id"] == inv["id"])
            .fold("0".to_string(), |a, l| {
                let (q, p) = (l["quantity_shipped"].as_str().unwrap(), l["unit_price"].as_str().unwrap());
                num(&mut c, "SELECT ($1::text::numeric + round($2::text::numeric * $3::text::numeric, 2))::text", &[&a, &q, &p])
            });
        let g = |k: &str| inv[k].as_str().unwrap().to_string();
        let legacy_delta = num(
            &mut c,
            "SELECT round($5::text::numeric - ($1::text::numeric + $2::text::numeric + $3::text::numeric - $4::text::numeric), 2)::text",
            &[&goods, &g("fuel_surcharge"), &g("tax"), &g("discount"), &g("total")],
        );
        let sid = e
            .s()
            .migration_lookup(T, "mjcc", "invoices", id)
            .unwrap()
            .unwrap();
        let row = c
            .query_one(
                &format!(
                    r#"SELECT goods_total::text, delta::text, reconciled FROM "{}".supplier_invoice WHERE id::text = $1"#,
                    e.schema
                ),
                &[&sid],
            )
            .unwrap();
        let (eng_goods, eng_delta, eng_reconciled): (String, String, bool) =
            (row.get(0), row.get(1), row.get(2));
        let o = e
            .s()
            .migration_check(
                T,
                &run,
                "invoice_goods_total",
                "invoice",
                id,
                &goods,
                &eng_goods,
                None,
            )
            .unwrap();
        assert!(
            o.pass,
            "invoice {id}: legacy goods {goods} engine {eng_goods}"
        );
        let o = e
            .s()
            .migration_check(
                T,
                &run,
                "invoice_delta",
                "invoice",
                id,
                &legacy_delta,
                &eng_delta,
                None,
            )
            .unwrap();
        assert!(
            o.pass,
            "invoice {id}: legacy delta {legacy_delta} engine {eng_delta}"
        );
        // The reconciled flag is recomputed, not copied: only the invoice that adds up is true.
        assert_eq!(eng_reconciled, legacy_delta == "0.00", "invoice {id}");
        passes += 2;
    }
    // Line totals: each line's quantity x price, summed over the whole table.
    let legacy_lines: String = t["invoice_items"].as_array().unwrap().iter().fold("0".to_string(), |a, l| {
        let (q, p) = (l["quantity_shipped"].as_str().unwrap(), l["unit_price"].as_str().unwrap());
        num(&mut c, "SELECT ($1::text::numeric + round($2::text::numeric * $3::text::numeric, 2))::text", &[&a, &q, &p])
    });
    let engine_lines: String = c
        .query_one(
            &format!(
                r#"SELECT coalesce(sum(ext_price),0)::text FROM "{}".supplier_invoice_line"#,
                e.schema
            ),
            &[],
        )
        .unwrap()
        .get(0);
    let o = e
        .s()
        .migration_check(
            T,
            &run,
            "invoice_line_total",
            "table",
            "invoice_items",
            &legacy_lines,
            &engine_lines,
            None,
        )
        .unwrap();
    assert!(o.pass, "legacy lines {legacy_lines} engine {engine_lines}");
    let _ = passes;
    assert_eq!(e.count("supplier_invoice_line"), 3);
}
