//! Runs the shared contract and typed-specific cases on a real Postgres (`CE_TEST_DATABASE_URL`).
use ce_store::{contract, Store, StoreError, TenantScope};
use ce_store_pg::TypedPgStore;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

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
    let m = json!({
      "manifest_version": 1,
      "template": {"name": "typed_test", "version": "0.1.0", "title": "Typed test"},
      "entities": {
        "item": {"fields": {"n": {"type": "integer"}}},
        "order": {"fields": {"n": {"type": "integer"}}},
        "part": {
          "fields": {
            "name": {"type": "string", "required": true, "max_length": 20, "unique": true},
            "price": {"type": "decimal", "precision": 10, "scale": 2, "default": 0},
            "kind": {"type": "enum", "values": ["a", "b"], "default": "a"},
            "owner": {"type": "ref", "ref": "item"}
          }
        },
        "ticket": {
          "fields": {"title": {"type": "string"}},
          "state": {"field": "state", "initial": "open", "states": {"open": {"code": 100, "to": ["closed"]}, "closed": {"code": 200}}},
          "soft_delete": false
        }
      }
    });
    ce_manifest::validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"))
}

fn fresh(url: &str) -> TypedPgStore {
    let name = format!(
        "ce_typed_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    TypedPgStore::install(url, &manifest(), &name).expect("install")
}

fn scope() -> TenantScope {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p =
        B64.encode(r#"{"sub":"u1","tenant":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","exp":2000}"#);
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
    ce_auth::Verifier::new(key)
        .unwrap()
        .verify_token(&token, 1000)
        .unwrap()
        .scope()
        .clone()
}

#[test]
fn typed_store_passes_the_contract() {
    let Some(url) = url() else { return };
    let made: std::cell::RefCell<Vec<String>> = Default::default();
    contract::run_all(&|| {
        let s = fresh(&url);
        made.borrow_mut().push(s.schema_name().to_string());
        s
    });
    for name in made.borrow().iter() {
        TypedPgStore::open(&url, &manifest(), name)
            .unwrap()
            .drop_schema()
            .unwrap();
    }
}

#[test]
fn defaults_decimals_and_required_fields() {
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    let r = s
        .insert(&sc, "part", json!({"name": "bolt"}), None)
        .unwrap();
    assert_eq!(r.data["price"], "0.00");
    assert_eq!(r.data["kind"], "a");
    let r = s
        .insert(&sc, "part", json!({"name": "nut", "price": "12.50"}), None)
        .unwrap();
    assert_eq!(s.get(&sc, "part", &r.id).unwrap().data["price"], "12.50");
    assert!(matches!(
        s.insert(&sc, "part", json!({}), None),
        Err(StoreError::BadData(_))
    ));
    s.drop_schema().unwrap();
}

#[test]
fn constraints_map_to_constraint_errors() {
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    s.insert(&sc, "part", json!({"name": "bolt"}), None)
        .unwrap();
    assert_eq!(
        s.insert(&sc, "part", json!({"name": "bolt"}), None),
        Err(StoreError::Constraint("unique"))
    );
    assert_eq!(
        s.insert(
            &sc,
            "part",
            json!({"name": "x", "owner": "11111111-1111-1111-1111-111111111111"}),
            None
        ),
        Err(StoreError::Constraint("reference"))
    );
    s.drop_schema().unwrap();
}

#[test]
fn state_is_engine_managed() {
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    let c = s
        .insert(&sc, "ticket", json!({"state": "closed"}), None)
        .unwrap();
    assert_eq!(c.data["state"], "open");
    let t = s
        .insert(&sc, "ticket", json!({"title": "a"}), None)
        .unwrap();
    assert_eq!(t.data["state"], "open");
    let u = s
        .update(
            &sc,
            "ticket",
            &t.id,
            1,
            json!({"title": "b", "state": "closed"}),
        )
        .unwrap();
    assert_eq!(u.data["state"], "open");
    assert_eq!(u.data["title"], "b");
    s.drop_schema().unwrap();
}

#[test]
fn hard_delete_when_soft_delete_is_off() {
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    let t = s.insert(&sc, "ticket", json!({}), None).unwrap();
    s.delete(&sc, "ticket", &t.id, 1).unwrap();
    assert_eq!(s.get(&sc, "ticket", &t.id), Err(StoreError::NotFound));
    s.drop_schema().unwrap();
}

#[test]
fn required_computed_fields_are_refused_at_install() {
    let Some(url) = url() else { return };
    let mut m = serde_json::to_value(&manifest().value).unwrap();
    m["entities"]["item"]["fields"]["total"] =
        json!({"type": "integer", "computed": "n * 2", "required": true});
    let m = ce_manifest::validate(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let name = format!(
        "ce_typed_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    assert!(matches!(
        TypedPgStore::install(&url, &m, &name),
        Err(StoreError::BadData(_))
    ));
}

#[test]
fn built_in_tools_install_and_run_on_the_typed_store() {
    let Some(url) = url() else { return };
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "shop", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let composed = ce_tools::compose(&shell, &["customers", "tasks", "notifications"]).unwrap();
    let m = ce_manifest::validate(composed.to_string().as_bytes()).unwrap();
    let name = format!(
        "ce_typed_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let s = TypedPgStore::install(&url, &m, &name).unwrap();
    let sc = scope();
    let c = s
        .insert(
            &sc,
            "customer",
            json!({"name": "Ada", "email": "ada@example.com"}),
            None,
        )
        .unwrap();
    assert_eq!(c.data["kind"], "person");
    s.insert(
        &sc,
        "customer_contact",
        json!({"customer": c.id, "name": "Bo"}),
        None,
    )
    .unwrap();
    let t = s
        .insert(
            &sc,
            "task",
            json!({"title": "call", "due_date": "2026-11-01"}),
            None,
        )
        .unwrap();
    assert_eq!(t.data["status"], "open");
    assert_eq!(t.data["priority"], "normal");
    let n = s
        .insert(
            &sc,
            "notification",
            json!({"recipient": "u1", "body": "hi"}),
            None,
        )
        .unwrap();
    assert_eq!(
        (n.data["status"].as_str(), n.data["channel"].as_str()),
        (Some("pending"), Some("in_app"))
    );
    s.drop_schema().unwrap();
}
