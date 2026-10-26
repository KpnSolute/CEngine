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
            "cost_budget": {"fields": {
                "period": {"type": "ref", "ref": "period", "required": true, "unique": true},
                "gov_allotment": {"type": "decimal", "precision": 12, "scale": 2, "required": true},
                "notes": {"type": "text"},
                "w1_planned_pull": {"type": "decimal", "precision": 12, "scale": 2},
                "w2_planned_pull": {"type": "decimal", "precision": 12, "scale": 2},
                "w3_planned_pull": {"type": "decimal", "precision": 12, "scale": 2},
                "w1_planned_renewable": {"type": "decimal", "precision": 12, "scale": 2},
                "w2_planned_renewable": {"type": "decimal", "precision": 12, "scale": 2},
                "w3_planned_renewable": {"type": "decimal", "precision": 12, "scale": 2}
            }},
            "budget_line_item": {"fields": {"active": {"default": true, "type": "boolean"}, "annual_budget": {"default": 0, "precision": 12, "scale": 2, "type": "decimal"}, "auto_source": {"type": "enum", "values": ["pulled", "received", "renewable", "snack_bar_revenue"]}, "description": {"max_length": 240, "required": true, "type": "string"}, "fy_start_year": {"indexed": true, "required": true, "type": "integer"}, "line_type": {"default": "cost", "indexed": true, "type": "enum", "values": ["cost", "revenue"]}, "sort_order": {"default": 0, "indexed": true, "type": "integer"}, "task_id": {"max_length": 60, "type": "string"}}, "sort_keys": ["sort_order"]},
            "budget_line_actual": {"fields": {"actual_amount": {"default": 0, "precision": 12, "scale": 2, "type": "decimal"}, "actual_key": {"computed": "concat(line_item, '|', period)", "indexed": true, "max_length": 100, "type": "string", "unique": true}, "line_item": {"indexed": true, "ref": "budget_line_item", "required": true, "type": "ref"}, "period": {"indexed": true, "ref": "period", "required": true, "type": "ref"}, "updated_by": {"max_length": 120, "type": "string"}}, "locked_when": "ref(period).status == 'closed'"},
            "menu_item": {"fields": {"active": {"default": true, "type": "boolean"}, "item_key": {"indexed": true, "max_length": 120, "required": true, "type": "string", "unique": true}, "name": {"indexed": true, "max_length": 160, "required": true, "searchable": true, "type": "string"}}, "sort_keys": ["name"]},
            "menu_cycle_day": {"fields": {"active": {"default": true, "type": "boolean"}, "cycle_day": {"indexed": true, "required": true, "type": "integer", "unique": true}, "cycle_week": {"indexed": true, "required": true, "type": "integer"}, "day_of_week": {"required": true, "type": "enum", "values": ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]}, "evening_service": {"max_length": 120, "type": "string"}, "midday_service": {"max_length": 120, "type": "string"}, "morning_service": {"max_length": 120, "type": "string"}, "zone": {"default": 1, "type": "integer"}}, "invariants": [{"expr": "cycle_day >= 1 and cycle_day <= 28", "message_code": "menu.cycle_day_range", "name": "cycle_day_range"}], "sort_keys": ["cycle_day"]},
            "menu_cycle_slot": {"fields": {"active": {"default": true, "type": "boolean"}, "cycle_day": {"indexed": true, "ref": "menu_cycle_day", "required": true, "type": "ref"}, "item": {"indexed": true, "ref": "menu_item", "required": true, "type": "ref"}, "meal_group": {"max_length": 60, "required": true, "type": "string"}, "meal_period": {"indexed": true, "max_length": 60, "required": true, "type": "string"}, "service_order": {"default": 1, "indexed": true, "type": "integer"}, "slot_name": {"max_length": 120, "required": true, "type": "string"}, "slot_order": {"default": 1, "indexed": true, "type": "integer"}, "updated_by": {"max_length": 120, "type": "string"}}, "sort_keys": ["service_order", "slot_order"]},
            "menu_suggestion": {"fields": {"cycle_day": {"type": "integer"}, "meal_period": {"max_length": 60, "type": "string"}, "notes": {"type": "text"}, "slot_name": {"max_length": 120, "type": "string"}, "source": {"default": "lunchvoice", "max_length": 60, "type": "string"}, "status": {"indexed": true, "type": "enum", "values": ["new", "reviewed", "adopted", "dismissed"]}, "submitted_by": {"max_length": 120, "pii": true, "type": "string"}, "suggested_item": {"max_length": 200, "required": true, "type": "string"}}, "sort_keys": ["status"], "state": {"field": "status", "initial": "new", "states": {"adopted": {"code": 600}, "dismissed": {"code": 610}, "new": {"code": 100, "to": ["reviewed", "dismissed"]}, "reviewed": {"code": 200, "to": ["adopted", "dismissed"]}}}},
            "snack_bar_product": {"fields": {"active": {"default": true, "type": "boolean"}, "name": {"indexed": true, "max_length": 120, "required": true, "type": "string"}, "price": {"default": 0, "precision": 10, "scale": 2, "type": "decimal"}, "stock_qty": {"default": 0, "type": "integer"}}, "invariants": [{"expr": "stock_qty >= 0", "message_code": "snack.negative_stock", "name": "stock_not_negative"}], "sort_keys": ["name"]},
            "snack_bar_rate": {"fields": {"discount_pct": {"default": 0, "precision": 5, "scale": 2, "type": "decimal"}, "entity_type": {"required": true, "type": "enum", "unique": true, "values": ["student", "staff"]}, "tax_pct": {"default": 0, "precision": 5, "scale": 2, "type": "decimal"}}, "invariants": [{"expr": "tax_pct >= 0 and tax_pct <= 100 and discount_pct >= 0 and discount_pct <= 100", "message_code": "snack.rate_range", "name": "pct_range"}]},
            "snack_bar_sale": {"fields": {"business_date": {"indexed": true, "required": true, "type": "date", "unique": true}, "closing_cash": {"default": 0, "precision": 12, "scale": 2, "type": "decimal"}, "opening_cash": {"default": 0, "precision": 12, "scale": 2, "type": "decimal"}, "recorded_by": {"max_length": 120, "type": "string"}, "register_sales": {"default": 0, "precision": 12, "scale": 2, "type": "decimal"}}, "sort_keys": ["business_date"]},
            "snack_bar_transaction": {"fields": {"business_date": {"indexed": true, "required": true, "type": "date"}, "discount_amount": {"computed": "round(subtotal * discount_pct / 100, 2)", "precision": 12, "scale": 2, "type": "decimal"}, "discount_pct": {"computed": "ref(rate).discount_pct", "precision": 5, "scale": 2, "type": "decimal"}, "entity_name": {"max_length": 160, "pii": true, "required": true, "type": "string"}, "entity_type": {"computed": "ref(rate).entity_type", "type": "enum", "values": ["student", "staff"]}, "rate": {"indexed": true, "ref": "snack_bar_rate", "required": true, "type": "ref"}, "recorded_by": {"max_length": 120, "type": "string"}, "subtotal": {"computed": "sum(children(snack_bar_transaction_item, transaction).line_total)", "precision": 12, "scale": 2, "type": "decimal"}, "tax_amount": {"computed": "round((subtotal - discount_amount) * tax_pct / 100, 2)", "precision": 12, "scale": 2, "type": "decimal"}, "tax_pct": {"computed": "ref(rate).tax_pct", "precision": 5, "scale": 2, "type": "decimal"}, "total_amount": {"computed": "round(subtotal - discount_amount + tax_amount, 2)", "precision": 12, "scale": 2, "type": "decimal"}}, "sort_keys": ["business_date"]},
            "snack_bar_transaction_item": {"fields": {"line_total": {"computed": "round(unit_price * qty, 2)", "precision": 12, "scale": 2, "type": "decimal"}, "product": {"indexed": true, "ref": "snack_bar_product", "type": "ref"}, "product_name": {"max_length": 120, "required": true, "type": "string"}, "qty": {"required": true, "type": "integer"}, "transaction": {"indexed": true, "ref": "snack_bar_transaction", "required": true, "type": "ref"}, "unit_price": {"precision": 12, "required": true, "scale": 2, "type": "decimal"}}, "invariants": [{"expr": "qty > 0", "message_code": "snack.qty", "name": "positive_qty"}]},
            "staff_member": {"fields": {
                "username": {"type": "string", "required": true, "unique": true, "max_length": 80},
                "display_name": {"type": "string", "max_length": 120},
                "role": {"type": "enum", "values": ["staff", "assistant", "manager", "admin", "sudo"], "required": true},
                "active": {"type": "boolean", "default": true}
            }, "sort_keys": ["username"]},
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
        ("price_review_queue", 3),
        ("week_status", 3),
        ("user_profiles", 3),
        ("cost_budgets", 1),
        ("menu_items", 2),
        ("menu_cycle_days", 2),
        ("menu_cycle_slots", 3),
        ("menu_suggestions", 3),
        ("budget_line_items", 2),
        ("budget_line_actuals", 1),
        ("snack_bar_products", 2),
        ("snack_bar_entity_rates", 2),
        ("snack_bar_transactions", 2),
        ("snack_bar_transaction_items", 3),
        ("snack_bar_sales", 1),
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

#[test]
fn price_history_keeps_state_decision_and_prices() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    e.s().migration_set_run_status(&run, "reconciling").unwrap();
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let rows = fx["tables"]["price_review_queue"].as_array().unwrap();
    // Count by status: the legacy status is the engine state, row for row.
    for status in ["applied", "pending", "dismissed", "auto_applied", "kept"] {
        let legacy = rows.iter().filter(|r| r["status"] == status).count();
        let engine: i64 = c
            .query_one(
                &format!(
                    r#"SELECT count(*) FROM "{}".price_observation WHERE status = $1"#,
                    e.schema
                ),
                &[&status],
            )
            .unwrap()
            .get(0);
        assert_eq!(engine as usize, legacy, "status {status}");
    }
    // Each row lands on the line of its item and its month (legacy months count from 0).
    for r in rows {
        let sid = e
            .s()
            .migration_lookup(T, "mjcc", "price_review_queue", r["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        let row = c
            .query_one(
                &format!(
                    r#"SELECT o.observed_price::text, coalesce(o.applied_price::text, ''), o.decision,
                              round(o.change * 100, 4)::text, i.sku
                       FROM "{s}".price_observation o
                       JOIN "{s}".stock_line l ON l.id = o.line
                       JOIN "{s}".stock_item i ON i.id = l.item
                       WHERE o.id::text = $1"#,
                    s = e.schema
                ),
                &[&sid],
            )
            .unwrap();
        let (obs, applied, decision, change, sku): (String, String, String, String, String) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        let cents = |v: &str| v.parse::<f64>().unwrap();
        assert_eq!(sku, r["sku"].as_str().unwrap());
        assert!((cents(&obs) - cents(r["observed_price"].as_str().unwrap())).abs() < 1e-9);
        match r["applied_price"].as_str() {
            Some(a) => assert!((cents(&applied) - cents(a)).abs() < 1e-9, "{}", r["id"]),
            None => assert_eq!(applied, ""),
        }
        let want = match r["status"].as_str().unwrap() {
            "auto_applied" | "applied" => "apply",
            "kept" | "pending" => "hold",
            _ => "skip",
        };
        assert_eq!(decision, want);
        // The change is recomputed by the engine; it must agree with what the legacy rule stored.
        assert!(
            (cents(&change) - cents(r["change_pct"].as_str().unwrap())).abs() < 1e-3,
            "{}: engine {change} legacy {}",
            r["id"],
            r["change_pct"]
        );
    }
    assert_eq!(e.count("price_observation"), 3);
}

#[test]
fn a_status_the_entity_does_not_declare_is_refused_by_name() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let mut fx = read_json("tests/migration/fixtures/normal_month.json");
    fx["tables"]["price_review_queue"][0]["status"] = serde_json::json!("archived");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    let report = load(&e, &run, &fx["tables"]);
    let c = &report["price_review_queue"];
    assert_eq!((c.written, c.rejected), (2, 1), "{c:?}");
    assert_eq!(e.count("price_observation"), 2);
    assert_eq!(e.count("migration_errors"), 1);
}

#[test]
fn week_locks_land_on_the_period_and_a_week_it_cannot_hold_is_refused() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let mut fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    let report = load(&e, &run, &fx["tables"]);
    assert_eq!(report["week_status"].written, 3);
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let row = c
        .query_one(
            &format!(
                r#"SELECT week_1_locked, week_2_locked, week_3_locked, week_4_locked FROM "{}".period"#,
                e.schema
            ),
            &[],
        )
        .unwrap();
    let flags: Vec<bool> = (0..4).map(|i| row.get(i)).collect();
    // Locked and published weeks are locked; an open week is not; an unlisted week stays open.
    assert_eq!(flags, [true, false, true, false]);
    let legacy_locked = fx["tables"]["week_status"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| matches!(w["status"].as_str(), Some("locked" | "published")))
        .count();
    assert_eq!(flags.iter().filter(|f| **f).count(), legacy_locked);
    // The change is on the audit trail under the import request id.
    let n: i64 = c
        .query_one(
            &format!(
                r#"SELECT count(*) FROM "{}".audit_event WHERE entity = 'period'
                   AND operation = 'update' AND request_id = 'legacy-import'"#,
                e.schema
            ),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(n, 3);
    // A second load changes nothing.
    let again = load(&e, &run, &fx["tables"]);
    assert_eq!(again["week_status"].written, 0);
    // The period holds weeks 1 to 4; a fifth is refused by name, not dropped.
    let e2 = Env::with(&url, &inventory_manifest());
    fx["tables"]["week_status"] = serde_json::json!([
        {"id": "w5", "month": 10, "year": 2026, "week": 5, "status": "locked", "locked_by": "u1",
         "locked_at": "2026-11-29T10:00:00Z", "created_at": "2026-11-01T00:00:00Z"}
    ]);
    let run2 = e2.run("rehearsal");
    e2.s().migration_set_run_status(&run2, "running").unwrap();
    let r2 = load(&e2, &run2, &fx["tables"]);
    assert_eq!(
        (r2["week_status"].written, r2["week_status"].rejected),
        (0, 1)
    );
    assert_eq!(e2.count("migration_errors"), 1);
}

#[test]
fn staff_load_with_their_roles_and_no_pin_reaches_the_target() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let rows = fx["tables"]["user_profiles"].as_array().unwrap();
    for r in rows {
        let row = c
            .query_one(
                &format!(
                    r#"SELECT role, active, display_name FROM "{}".staff_member WHERE username = $1"#,
                    e.schema
                ),
                &[&r["username"].as_str().unwrap()],
            )
            .unwrap();
        let (role, active, name): (String, bool, String) = (row.get(0), row.get(1), row.get(2));
        assert_eq!(role, r["role"].as_str().unwrap());
        assert_eq!(active, r["active"].as_bool().unwrap());
        assert_eq!(name, r["display_name"].as_str().unwrap());
    }
    // No PIN, in any table of the target schema, in any column.
    let tables: Vec<String> = c
        .query(
            "SELECT table_name::text FROM information_schema.tables WHERE table_schema = $1 AND table_type = 'BASE TABLE'",
            &[&e.schema],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(tables.len() > 10);
    for r in rows {
        let pin = r["pin"].as_str().unwrap();
        for t in &tables {
            let n: i64 = c
                .query_one(
                    &format!(
                        r#"SELECT count(*) FROM "{}"."{t}" x WHERE row_to_json(x)::text LIKE $1"#,
                        e.schema
                    ),
                    &[&format!("%{pin}%")],
                )
                .unwrap()
                .get(0);
            assert_eq!(n, 0, "a PIN reached {t}");
        }
    }
}

#[test]
fn budgets_keep_the_allotment_and_every_weekly_plan_to_the_cent() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    e.s().migration_set_run_status(&run, "reconciling").unwrap();
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let b = &fx["tables"]["cost_budgets"][0];
    for col in [
        "gov_allotment",
        "w1_planned_pull",
        "w2_planned_pull",
        "w3_planned_pull",
        "w1_planned_renewable",
        "w2_planned_renewable",
        "w3_planned_renewable",
    ] {
        let legacy = b[col].as_str().unwrap();
        let engine: String = c
            .query_one(
                &format!(r#"SELECT {col}::text FROM "{}".cost_budget"#, e.schema),
                &[],
            )
            .unwrap()
            .get(0);
        let o = e
            .s()
            .migration_check(T, &run, col, "cost_budget", "b1", legacy, &engine, None)
            .unwrap();
        assert!(o.pass, "{col}: legacy {legacy} engine {engine}");
    }
    // The budget sits on the period of its month (legacy months count from 0).
    let label: String = c
        .query_one(
            &format!(
                r#"SELECT p.label FROM "{s}".cost_budget b JOIN "{s}".period p ON p.id = b.period"#,
                s = e.schema
            ),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(label, "2026-11");
}

#[test]
fn each_stock_line_carries_the_par_level_its_item_had() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let rows = c
        .query(
            &format!(
                r#"SELECT i.sku, l.par_level::text, i.par_level::text
                   FROM "{s}".stock_line l JOIN "{s}".stock_item i ON i.id = l.item"#,
                s = e.schema
            ),
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        let (sku, line, item): (String, String, String) = (r.get(0), r.get(1), r.get(2));
        assert!(!item.is_empty(), "{sku}: item has no par level");
        assert_eq!(line, item, "{sku}: line par {line} item par {item}");
    }
}

fn loaded() -> Option<(Env, String, serde_json::Value)> {
    let url = url()?;
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let run = e.run("rehearsal");
    e.s().migration_set_run_status(&run, "running").unwrap();
    load(&e, &run, &fx["tables"]);
    e.s().migration_set_run_status(&run, "reconciling").unwrap();
    Some((e, run, fx))
}

#[test]
fn snack_bar_totals_are_recomputed_by_the_engine_and_match_each_sale_to_the_cent() {
    let Some((e, run, fx)) = loaded() else { return };
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    for t in fx["tables"]["snack_bar_transactions"].as_array().unwrap() {
        let (id, who) = (
            t["id"].as_str().unwrap(),
            t["entity_name"].as_str().unwrap(),
        );
        for col in ["subtotal", "discount_amount", "tax_amount", "total_amount"] {
            let legacy = t[col].as_str().unwrap();
            let engine: String = c
                .query_one(
                    &format!(
                        r#"SELECT {col}::text FROM "{}".snack_bar_transaction WHERE entity_name = $1"#,
                        e.schema
                    ),
                    &[&who],
                )
                .unwrap()
                .get(0);
            let o = e
                .s()
                .migration_check(
                    T,
                    &run,
                    col,
                    "snack_bar_transaction",
                    id,
                    legacy,
                    &engine,
                    None,
                )
                .unwrap();
            assert!(o.pass, "{id} {col}: legacy {legacy} engine {engine}");
        }
    }
    // Item line totals are recomputed from unit price and quantity as well.
    let lines: String = c
        .query_one(
            &format!(
                r#"SELECT coalesce(sum(line_total),0)::text FROM "{}".snack_bar_transaction_item"#,
                e.schema
            ),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(lines, "10.05");
    // The day's register sale and cash are copied.
    let sale: (String, String) = {
        let r = c
            .query_one(
                &format!(
                    r#"SELECT register_sales::text, closing_cash::text FROM "{}".snack_bar_sale"#,
                    e.schema
                ),
                &[],
            )
            .unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!(sale, ("9.90".to_string(), "59.90".to_string()));
}

#[test]
fn the_cycle_menu_keeps_every_slot_on_its_day_and_item_and_each_suggestion_its_state() {
    let Some((e, _run, _fx)) = loaded() else {
        return;
    };
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let slots: Vec<(i32, String, bool)> = c
        .query(
            &format!(
                r#"SELECT d.cycle_day::int, i.item_key, s.active FROM "{s}".menu_cycle_slot s
                   JOIN "{s}".menu_cycle_day d ON d.id = s.cycle_day
                   JOIN "{s}".menu_item i ON i.id = s.item ORDER BY s.slot_name, d.cycle_day, i.item_key"#,
                s = e.schema
            ),
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        slots,
        vec![
            (1, "synthetic-stew".to_string(), true),
            (2, "synthetic-stew".to_string(), false),
            (1, "synthetic-salad".to_string(), true),
        ]
    );
    let states: Vec<(String, String)> = c
        .query(
            &format!(
                r#"SELECT suggested_item, status FROM "{}".menu_suggestion ORDER BY suggested_item"#,
                e.schema
            ),
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        states,
        vec![
            ("Synthetic Chili".to_string(), "new".to_string()),
            ("Synthetic Pie".to_string(), "dismissed".to_string()),
            ("Synthetic Soup".to_string(), "adopted".to_string()),
        ]
    );
}

#[test]
fn budget_lines_and_their_monthly_actuals_keep_amounts_and_land_on_the_right_period() {
    let Some((e, run, fx)) = loaded() else { return };
    let mut c = connect(&e.url, &Tls::Off).unwrap();
    for l in fx["tables"]["budget_line_items"].as_array().unwrap() {
        let (id, desc) = (
            l["id"].as_str().unwrap(),
            l["description"].as_str().unwrap(),
        );
        let legacy = l["annual_budget"].as_str().unwrap();
        let engine: String = c
            .query_one(
                &format!(
                    r#"SELECT annual_budget::text FROM "{}".budget_line_item WHERE description = $1"#,
                    e.schema
                ),
                &[&desc],
            )
            .unwrap()
            .get(0);
        let o = e
            .s()
            .migration_check(
                T,
                &run,
                "annual_budget",
                "budget_line_item",
                id,
                legacy,
                &engine,
                None,
            )
            .unwrap();
        assert!(o.pass, "{id}: legacy {legacy} engine {engine}");
    }
    // The actual was keyed to month 10 (0-based), which is the November period.
    let (amount, label): (String, String) = {
        let r = c
            .query_one(
                &format!(
                    r#"SELECT a.actual_amount::text, p.label FROM "{s}".budget_line_actual a
                       JOIN "{s}".period p ON p.id = a.period"#,
                    s = e.schema
                ),
                &[],
            )
            .unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!((amount.as_str(), label.as_str()), ("412.35", "2026-11"));
}

fn drive(
    e: &Env,
    checks: &serde_json::Value,
    tables: &serde_json::Value,
) -> ce_store_pg::rehearsal::Rehearsal {
    let plan = read_json("tests/migration/plans/mjcc_inventory_v1.json");
    let m = inventory_manifest();
    ce_store_pg::rehearsal::run(
        e.s(),
        &scope(),
        &e.schema,
        &ce_store_pg::rehearsal::Params {
            url: &e.url,
            tls: &Tls::Off,
            tenant_id: T,
            installation_id: "inst",
            source_system: "mjcc",
            source_ref: "synthetic",
            migration_version: "1",
            initiated_by: "rehearsal",
            default_actor: "legacy",
            now_unix: 1_700_000_000,
        },
        &plan,
        tables.as_object().unwrap(),
        checks,
        &|| ce_server::settle_imported(e.s(), &m, &scope(), 1_700_000_000).map(|_| ()),
    )
    .unwrap()
}

#[test]
fn the_driver_loads_reconciles_and_passes_a_run_only_when_every_check_is_zero() {
    let Some(url) = url() else { return };
    let e = Env::with(&url, &inventory_manifest());
    let fx = read_json("tests/migration/fixtures/normal_month.json");
    let checks = read_json("tests/migration/checks/mjcc_checks_v1.json");
    let r = drive(&e, &checks, &fx["tables"]);
    for c in &r.checks {
        assert!(c.pass, "{}: legacy {} target {}", c.key, c.legacy, c.target);
    }
    assert_eq!(r.checks.len(), checks.as_array().unwrap().len());
    assert_eq!(r.status, "passed");
    // A second drive resolves every row to the same target and the run still reconciles.
    let again = drive(&e, &checks, &fx["tables"]);
    assert_eq!(again.status, "passed");
    assert!(again
        .load
        .values()
        .all(|c| c.written == 0 && c.rejected == 0));
}

#[test]
fn the_driver_fails_the_run_on_a_one_cent_difference_and_on_a_rejected_row() {
    let Some(url) = url() else { return };
    let checks = read_json("tests/migration/checks/mjcc_checks_v1.json");
    // A legacy sale total that the engine's own arithmetic cannot reproduce does fail.
    let e2 = Env::with(&url, &inventory_manifest());
    let mut fx2 = read_json("tests/migration/fixtures/normal_month.json");
    fx2["tables"]["snack_bar_transactions"][0]["total_amount"] = serde_json::json!("7.64");
    let r2 = drive(&e2, &checks, &fx2["tables"]);
    let failed: Vec<_> = r2
        .checks
        .iter()
        .filter(|c| !c.pass)
        .map(|c| c.key.as_str())
        .collect();
    assert_eq!(failed, vec!["snack_bar_sale_total"]);
    assert_eq!(r2.status, "failed");
    // A row whose parent is missing is rejected and keeps the run from passing.
    let e3 = Env::with(&url, &inventory_manifest());
    let mut fx3 = read_json("tests/migration/fixtures/normal_month.json");
    fx3["tables"]["menu_cycle_slots"][0]["item_id"] = serde_json::json!("nope");
    let r3 = drive(&e3, &checks, &fx3["tables"]);
    assert_eq!(r3.load["menu_cycle_slots"].rejected, 1);
    assert_eq!(r3.status, "failed");
}
