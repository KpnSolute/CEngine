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
            "name": {"type": "string", "required": true, "max_length": 20, "unique": true, "indexed": true},
            "price": {"type": "decimal", "precision": 10, "scale": 2, "default": 0, "indexed": true},
            "kind": {"type": "enum", "values": ["a", "b"], "default": "a"},
            "owner": {"type": "ref", "ref": "item"},
            "rank": {"type": "integer", "indexed": true}
          },
          "sort_keys": ["name", "price", "rank"]
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
    scope_of("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
}

fn scope_of(tenant: &str) -> TenantScope {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(r#"{{"sub":"u1","tenant":"{tenant}","exp":2000}}"#));
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
fn list_pages_sorts_filters_and_isolates_tenants() {
    use ce_store::{ListRequest, QueryError, SortDir, SortKey};
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    let other = scope_of("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    for (n, p) in [
        ("e", "5.50"),
        ("a", "10.00"),
        ("c", "2.25"),
        ("b", "10.00"),
        ("d", "7"),
    ] {
        s.insert(&sc, "part", json!({"name": n, "price": p}), None)
            .unwrap();
    }
    s.insert(&other, "part", json!({"name": "zz"}), None)
        .unwrap();
    let by = |f: &str| {
        vec![SortKey {
            field: f.into(),
            dir: SortDir::Asc,
        }]
    };

    // Numeric (not text) order: 2.25, 5.50, 7.00, 10.00, 10.00. Ties break on id.
    let mut req = ListRequest {
        sort: by("price"),
        limit: Some(2),
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = s.list(&sc, "part", &req).unwrap();
        seen.extend(
            page.records
                .iter()
                .map(|r| r.data["price"].as_str().unwrap().to_string()),
        );
        match page.next {
            Some(c) => req.after = Some(c),
            None => break,
        }
    }
    assert_eq!(seen, ["2.25", "5.50", "7.00", "10.00", "10.00"]);

    // Filter, select, and tenant scoping.
    let page = s
        .list(
            &sc,
            "part",
            &ListRequest {
                select: Some(vec!["name".into()]),
                filter: Some(json!({"field": "name", "op": "eq", "value": "c"})),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(page.records[0].data["name"], "c");
    assert!(page.records[0].data.get("price").is_none());
    let all = s.list(&sc, "part", &ListRequest::default()).unwrap();
    assert_eq!(all.records.len(), 5);
    assert!(all.records.iter().all(|r| r.data["name"] != "zz"));

    // Deleted rows disappear; bad queries are refused.
    let id = all.records[0].id.clone();
    s.delete(&sc, "part", &id, 1).unwrap();
    assert_eq!(
        s.list(&sc, "part", &ListRequest::default())
            .unwrap()
            .records
            .len(),
        4
    );
    assert!(matches!(
        s.list(
            &sc,
            "part",
            &ListRequest {
                sort: by("kind"),
                ..Default::default()
            }
        ),
        Err(StoreError::Query(QueryError::UnindexedSort(_)))
    ));
    s.drop_schema().unwrap();
}

#[test]
fn paging_handles_null_sort_values_mixed_directions_and_hidden_sort_fields() {
    use ce_store::{ListRequest, SortDir, SortKey};
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    // `owner` is optional; no item exists for it, so use `part.kind` (default a) and price.
    // Prices: a=1, b=1, c=2, d=2, e=3. Two rows have no name filter issue: names are unique.
    for (n, p) in [("a", "1"), ("b", "1"), ("c", "2"), ("d", "2"), ("e", "3")] {
        s.insert(&sc, "part", json!({"name": n, "price": p}), None)
            .unwrap();
    }
    let key = |f: &str, dir| SortKey {
        field: f.into(),
        dir,
    };
    // price DESC, name ASC, two per page: e, then c d, then a b.
    let mut req = ListRequest {
        sort: vec![key("price", SortDir::Desc), key("name", SortDir::Asc)],
        select: Some(vec!["name".into()]),
        limit: Some(2),
        ..Default::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = s.list(&sc, "part", &req).unwrap();
        for r in &page.records {
            // `price` was read for the cursor only and is not returned.
            assert!(r.data.get("price").is_none());
            seen.push(r.data["name"].as_str().unwrap().to_string());
        }
        match page.next {
            Some(c) => req.after = Some(c),
            None => break,
        }
    }
    assert_eq!(seen, ["e", "c", "d", "a", "b"]);
    s.drop_schema().unwrap();
}

#[test]
fn paging_through_null_sort_values_loses_and_repeats_nothing() {
    use ce_store::{ListRequest, SortDir, SortKey};
    let Some(url) = url() else { return };
    let s = fresh(&url);
    let sc = scope();
    // rank: a=2, b=null, c=1, d=null, e=2, f=null. Nulls sort last in both directions.
    for (n, r) in [
        ("a", Some(2)),
        ("b", None),
        ("c", Some(1)),
        ("d", None),
        ("e", Some(2)),
        ("f", None),
    ] {
        let mut d = json!({"name": n});
        if let Some(r) = r {
            d["rank"] = json!(r);
        }
        s.insert(&sc, "part", d, None).unwrap();
    }
    for (dir, _) in [
        (SortDir::Asc, ["c", "a", "e"]),
        (SortDir::Desc, ["a", "e", "c"]),
    ] {
        let mut req = ListRequest {
            sort: vec![SortKey {
                field: "rank".into(),
                dir,
            }],
            select: Some(vec!["name".into()]),
            limit: Some(2),
            ..Default::default()
        };
        let mut seen = Vec::new();
        loop {
            let page = s.list(&sc, "part", &req).unwrap();
            seen.extend(
                page.records
                    .iter()
                    .map(|r| r.data["name"].as_str().unwrap().to_string()),
            );
            match page.next {
                Some(c) => req.after = Some(c),
                None => break,
            }
        }
        assert_eq!(seen.len(), 6, "{seen:?}");
        let mut all = seen.clone();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 6, "{seen:?}");
        // The three ranked rows come first (ties on rank 2 break by id), then the unranked.
        let mut head = seen[..3].to_vec();
        head.sort();
        assert_eq!(head, ["a", "c", "e"], "{seen:?}");
        let c_at = seen.iter().position(|n| n == "c").unwrap();
        assert_eq!(c_at, if dir == SortDir::Asc { 0 } else { 2 }, "{seen:?}");
    }
    s.drop_schema().unwrap();
}
