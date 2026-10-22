//! Audit preservation (P6): an imported record keeps its legacy actor and time in the audit
//! trail, the migrator is never the actor, the audit event names no run, and the record, its id
//! mapping and its audit event are written together or not at all.
use ce_auth::Verifier;
use ce_manifest::validate;
use ce_store_pg::import::{ImportRecord, IMPORT_REQUEST_ID};
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
    let m = ce_tools::compose(
        &json!({
            "manifest_version": 1,
            "template": {"name": "shop", "version": "0.1.0", "title": "Shop"},
            "entities": {"doc": {"fields": {"title": {"type": "string", "required": true}}}},
            "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
        }),
        &["audit"],
    )
    .unwrap();
    validate(m.to_string().as_bytes()).unwrap()
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

const ACTOR: &str = "legacy.jane";
const AT: i64 = 1_600_000_000;

fn rec<'a>(run: &'a str, id: &'a str, data: serde_json::Value) -> ImportRecord<'a> {
    ImportRecord {
        run,
        source_system: "mjcc",
        source_table: "docs",
        source_id: id,
        source_parent_id: None,
        entity: "doc",
        data,
        actor: ACTOR,
        at: AT,
    }
}

fn scope() -> ce_store::TenantScope {
    Verifier::new(b"0123456789abcdef0123456789abcdef")
        .unwrap()
        .public_scope(T)
        .unwrap()
}

#[test]
fn an_imported_record_keeps_the_legacy_actor_and_time() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let run = e.run("migrator-service");
    let out = e
        .s()
        .migration_import(&scope(), &rec(&run, "7", json!({"title": "lease"})))
        .unwrap();
    assert!(out.created);

    let mut c = connect(&e.url, &Tls::Off).unwrap();
    let s = &e.schema;
    let row = c
        .query_one(
            &format!(r#"SELECT extract(epoch FROM created_at)::bigint, extract(epoch FROM updated_at)::bigint FROM "{s}".doc WHERE id = $1::text::uuid"#),
            &[&out.target_id],
        )
        .unwrap();
    assert_eq!(
        (row.get::<_, i64>(0), row.get::<_, i64>(1)),
        (AT, AT),
        "the record is as old as it was"
    );

    let ev = c
        .query_one(
            &format!(
                r#"SELECT row_to_json(a)::text FROM "{s}".audit_event a WHERE record_id = $1"#
            ),
            &[&out.target_id],
        )
        .unwrap();
    let text: String = ev.get(0);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["actor"], ACTOR);
    assert_eq!(v["at"], AT);
    assert_eq!(v["operation"], "create");
    assert_eq!(v["request_id"], IMPORT_REQUEST_ID);
    assert!(
        !text.contains(&run),
        "the audit event must not name the migration run: {text}"
    );
    assert!(
        !text.contains("migrator-service"),
        "the migrator is never the actor: {text}"
    );

    // The run is recorded in the id map, not in the audit trail.
    let mapped: String = c
        .query_one(
            &format!(r#"SELECT migration_run_id::text FROM "{s}".migration_id_map WHERE target_id = $1::text::uuid"#),
            &[&out.target_id],
        )
        .unwrap()
        .get(0);
    assert_eq!(mapped, run);
}

#[test]
fn importing_again_changes_nothing() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let (r1, r2) = (e.run("a"), e.run("b"));
    let first = e
        .s()
        .migration_import(&scope(), &rec(&r1, "7", json!({"title": "lease"})))
        .unwrap();
    let again = e
        .s()
        .migration_import(&scope(), &rec(&r2, "7", json!({"title": "lease"})))
        .unwrap();
    assert!(first.created && !again.created);
    assert_eq!(first.target_id, again.target_id);
    assert_eq!(
        (
            e.count("doc"),
            e.count("audit_event"),
            e.count("migration_id_map")
        ),
        (1, 1, 1)
    );
}

#[test]
fn a_record_the_rules_refuse_leaves_no_record_mapping_or_audit_event() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let run = e.run("t");
    // `title` is required.
    assert!(e
        .s()
        .migration_import(&scope(), &rec(&run, "8", json!({})))
        .is_err());
    assert_eq!(
        (
            e.count("doc"),
            e.count("audit_event"),
            e.count("migration_id_map")
        ),
        (0, 0, 0)
    );
}

#[test]
fn a_source_id_cannot_be_imported_into_another_entity() {
    let Some(url) = url() else { return };
    let e = Env::new(&url);
    let run = e.run("t");
    e.s()
        .migration_import(&scope(), &rec(&run, "7", json!({"title": "x"})))
        .unwrap();
    let mut other = rec(&run, "7", json!({"title": "x"}));
    other.entity = "audit_event";
    assert!(e.s().migration_import(&scope(), &other).is_err());
    assert_eq!(e.count("doc"), 1);
}
