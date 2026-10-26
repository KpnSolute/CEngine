//! A second run duplicates nothing and resolves the same target ids; a source id keeps one entity.
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

const TABLES: [(&str, &str, u32); 3] = [
    ("inventory_categories", "inventory_category", 4),
    ("vendors", "vendor", 6),
    ("inventory_items", "stock_item", 40),
];

fn load(e: &Env, run: &str) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    for (table, entity, n) in TABLES {
        for i in 1..=n {
            let id = format!("{i}");
            let parent = (table == "inventory_items").then(|| format!("{}", (i % 4) + 1));
            let m = e
                .s()
                .migration_map_source(T, run, "mjcc", table, &id, entity, parent.as_deref())
                .unwrap();
            out.push((table.to_string(), id, m.target_id, m.created));
        }
    }
    out
}

#[test]
fn a_second_run_resolves_the_same_targets_and_creates_nothing() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let (r1, r2) = (e.run("first"), e.run("second"));

    let first = load(&e, &r1);
    assert!(
        first.iter().all(|r| r.3),
        "every row is new on the first run"
    );
    let rows_after_first = e.count("migration_id_map");
    assert_eq!(rows_after_first, 50);

    let second = load(&e, &r2);
    assert!(
        second.iter().all(|r| !r.3),
        "nothing is created on the second run"
    );
    assert_eq!(
        e.count("migration_id_map"),
        rows_after_first,
        "zero new rows"
    );
    for (a, b) in first.iter().zip(&second) {
        assert_eq!((&a.0, &a.1, &a.2), (&b.0, &b.1, &b.2), "same target id");
    }
    let ids: std::collections::HashSet<_> = first.iter().map(|r| r.2.clone()).collect();
    assert_eq!(ids.len(), first.len(), "no two source rows share a target");
}

#[test]
fn a_source_id_cannot_move_to_another_entity() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let r = e.run("t");
    e.s()
        .migration_map_source(T, &r, "mjcc", "vendors", "7", "vendor", None)
        .unwrap();
    let again = e
        .s()
        .migration_map_source(T, &r, "mjcc", "vendors", "7", "location", None);
    assert!(again.is_err(), "the lineage keeps its first entity");
    assert_eq!(e.count("migration_id_map"), 1);
}
