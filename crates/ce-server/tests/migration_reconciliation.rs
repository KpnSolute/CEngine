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
        let schema = format!(
            "ce_mg_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        );
        let store = TypedPgStore::install(url, &manifest(), &schema).unwrap();
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
