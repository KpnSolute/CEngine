//! A released synthetic dataset migrates within a recorded ceiling.
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

/// Mappings written by the performance test: about a year of a mid-size cafeteria's rows.
const ROWS: u32 = 10_000;
/// The ceiling, recorded: ~3 s on the CI runner's loopback Postgres, so 10x headroom.
const CEILING_SECS: u64 = 60;

#[test]
fn ten_thousand_mappings_finish_inside_the_recorded_ceiling() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let r = e.run("perf");
    let t0 = std::time::Instant::now();
    for i in 0..ROWS {
        e.s()
            .migration_map_source(
                T,
                &r,
                "mjcc",
                "inventory_transactions",
                &i.to_string(),
                "stock_movement",
                None,
            )
            .unwrap();
    }
    let first = t0.elapsed();
    let t1 = std::time::Instant::now();
    for i in 0..ROWS {
        let m = e
            .s()
            .migration_map_source(
                T,
                &r,
                "mjcc",
                "inventory_transactions",
                &i.to_string(),
                "stock_movement",
                None,
            )
            .unwrap();
        assert!(!m.created);
    }
    let second = t1.elapsed();
    eprintln!("migration_performance: {ROWS} rows, first run {first:?}, rerun {second:?}, ceiling {CEILING_SECS}s");
    assert_eq!(e.count("migration_id_map"), ROWS as i64);
    assert!(first.as_secs() < CEILING_SECS, "first run took {first:?}");
    assert!(second.as_secs() < CEILING_SECS, "rerun took {second:?}");
}
