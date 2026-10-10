use ce_manifest::{validate, Manifest};
use ce_query::*;
use serde_json::{json, Value};

fn manifest(name: &str) -> Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    validate(std::fs::read_to_string(path).unwrap().as_bytes()).expect("example is valid")
}

fn target(entity: &str) -> Target {
    Target {
        schema: "ce_t1".into(),
        entity: entity.into(),
    }
}

fn scope() -> TenantScope {
    TenantScope::new("11111111-1111-1111-1111-111111111111")
}

fn plan(m: &Manifest, entity: &str, req: &ListRequest) -> Result<Planned, QueryError> {
    plan_list(m, &target(entity), &scope(), req)
}

fn placeholders(sql: &str) -> usize {
    let mut max = 0;
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                max = max.max(sql[i + 1..j].parse::<usize>().unwrap());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    max
}

fn assert_tenant_scoped(p: &Planned) {
    assert!(
        p.sql.contains("WHERE \"tenant_id\" = $1::uuid"),
        "missing tenant predicate: {}",
        p.sql
    );
    assert_eq!(
        p.params[0],
        Param::Text("11111111-1111-1111-1111-111111111111".into())
    );
    assert_eq!(
        placeholders(&p.sql),
        p.params.len(),
        "placeholders and params disagree: {}",
        p.sql
    );
}

#[test]
fn plain_list_is_tenant_scoped_and_paged() {
    let m = manifest("booking_crm");
    let p = plan(&m, "booking", &ListRequest::default()).unwrap();
    assert_tenant_scoped(&p);
    assert!(p.sql.contains("FROM \"ce_t1\".\"booking\""));
    assert!(p.sql.contains("\"deleted_at\" IS NULL"));
    assert!(p.sql.ends_with("LIMIT 51"), "{}", p.sql);
}

#[test]
fn or_filter_cannot_escape_the_tenant_predicate() {
    let m = manifest("booking_crm");
    let f = json!({"or": [{"field":"status","op":"eq","value":"confirmed"}, {"field":"status","op":"eq","value":"requested"}]});
    let p = plan(
        &m,
        "booking",
        &ListRequest {
            filter: Some(f),
            ..Default::default()
        },
    )
    .unwrap();
    assert_tenant_scoped(&p);
    assert!(
        p.sql
            .contains("AND ((\"status\" = $2) OR (\"status\" = $3))"),
        "{}",
        p.sql
    );
}

#[test]
fn values_are_bound_never_spliced() {
    let m = manifest("booking_crm");
    let evil = "x'; DROP TABLE booking; --";
    let f = json!({"field":"notes","op":"like","value":evil});
    let p = plan(
        &m,
        "booking",
        &ListRequest {
            filter: Some(f),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!p.sql.contains("DROP"));
    assert!(p.params.contains(&Param::Text(evil.into())));
}

#[test]
fn unknown_and_hostile_identifiers_are_rejected() {
    let m = manifest("booking_crm");
    for bad in [
        "nope",
        "status\"; DROP TABLE x; --",
        "tenant_id",
        "deleted_at",
        "1=1",
    ] {
        let f = json!({"field": bad, "op":"eq","value":"x"});
        let r = plan(
            &m,
            "booking",
            &ListRequest {
                filter: Some(f),
                ..Default::default()
            },
        );
        assert_eq!(r, Err(QueryError::UnknownField(bad.into())), "field {bad}");
    }
    assert_eq!(
        plan(&m, "ghost", &ListRequest::default()),
        Err(QueryError::UnknownEntity("ghost".into()))
    );
    let r = plan_list(
        &m,
        &Target {
            schema: "x\"; --".into(),
            entity: "booking".into(),
        },
        &scope(),
        &ListRequest::default(),
    );
    assert_eq!(r, Err(QueryError::BadSchema));
}

#[test]
fn select_is_limited_to_declared_fields() {
    let m = manifest("booking_crm");
    let ok = plan(
        &m,
        "booking",
        &ListRequest {
            select: Some(vec!["starts_at".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ok.sql.starts_with("SELECT \"id\", \"starts_at\" FROM"));
    let bad = plan(
        &m,
        "booking",
        &ListRequest {
            select: Some(vec!["tenant_id".into()]),
            ..Default::default()
        },
    );
    assert_eq!(bad, Err(QueryError::UnknownField("tenant_id".into())));
}

#[test]
fn only_indexed_sort_keys_are_allowed_and_limit_is_capped() {
    let m = manifest("booking_crm");
    let sort = |f: &str| {
        vec![SortKey {
            field: f.into(),
            dir: SortDir::Desc,
        }]
    };
    let ok = plan(
        &m,
        "booking",
        &ListRequest {
            sort: sort("starts_at"),
            limit: Some(10_000),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(ok.sql.contains("ORDER BY \"starts_at\" DESC, \"id\" DESC"));
    assert!(ok.sql.ends_with(&format!("LIMIT {}", MAX_LIMIT + 1)));
    let bad = plan(
        &m,
        "booking",
        &ListRequest {
            sort: sort("notes"),
            ..Default::default()
        },
    );
    assert_eq!(bad, Err(QueryError::UnindexedSort("notes".into())));
}

#[test]
fn keyset_cursor_uses_row_comparison_with_bound_values() {
    let m = manifest("booking_crm");
    let req = ListRequest {
        sort: vec![SortKey {
            field: "starts_at".into(),
            dir: SortDir::Asc,
        }],
        after: Some(vec![
            json!("2026-10-10T12:00:00Z"),
            json!("22222222-2222-2222-2222-222222222222"),
        ]),
        ..Default::default()
    };
    let p = plan(&m, "booking", &req).unwrap();
    assert_tenant_scoped(&p);
    assert!(
        p.sql
            .contains("(ROW(\"starts_at\", \"id\") > ROW($2::timestamptz, $3::uuid))"),
        "{}",
        p.sql
    );
    let wrong_len = ListRequest {
        after: Some(vec![json!("x")]),
        ..req.clone()
    };
    assert_eq!(plan(&m, "booking", &wrong_len), Err(QueryError::BadCursor));
    let mixed = ListRequest {
        sort: vec![
            SortKey {
                field: "starts_at".into(),
                dir: SortDir::Asc,
            },
            SortKey {
                field: "id".into(),
                dir: SortDir::Desc,
            },
        ],
        after: Some(vec![
            json!("2026-10-10T12:00:00Z"),
            json!("22222222-2222-2222-2222-222222222222"),
        ]),
        ..Default::default()
    };
    assert_eq!(
        plan(&m, "booking", &mixed),
        Err(QueryError::MixedSortCursor)
    );
}

#[test]
fn typed_values_are_checked() {
    let m = manifest("inventory");
    let f = |field: &str, op: &str, v: Value| json!({"field": field, "op": op, "value": v});
    let run = |filter: Value| {
        plan(
            &m,
            "item",
            &ListRequest {
                filter: Some(filter),
                ..Default::default()
            },
        )
    };
    assert!(run(f("unit_cost", "gte", json!("1.50")))
        .unwrap()
        .sql
        .contains("\"unit_cost\" >= $2::numeric"));
    assert!(matches!(
        run(f("unit_cost", "gte", json!(1.5))),
        Err(QueryError::BadValue(_))
    ));
    assert!(matches!(
        run(f("unit_cost", "gte", json!("1; DROP"))),
        Err(QueryError::BadValue(_))
    ));
    assert!(matches!(
        run(f("category", "eq", json!("weapons"))),
        Err(QueryError::BadValue(_))
    ));
    assert!(matches!(
        run(f("category", "gt", json!("dry"))),
        Err(QueryError::BadOp(_, _))
    ));
    assert!(matches!(
        run(f("name", "bogus", json!("x"))),
        Err(QueryError::BadOp(_, _))
    ));
    assert!(matches!(
        run(f("period", "eq", json!("not-a-uuid-but-bound"))),
        Ok(_)
    ));
    let nested = (0..20).fold(f("name", "eq", json!("x")), |acc, _| json!({"not": acc}));
    assert_eq!(run(nested), Err(QueryError::FilterTooLarge));
}

// --- fuzz: random filters must never produce SQL without the tenant predicate ---

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[(self.next() as usize) % v.len()]
    }
}

fn random_filter(r: &mut Rng, depth: usize) -> Value {
    let fields = [
        "status",
        "notes",
        "starts_at",
        "contact",
        "bogus",
        "tenant_id",
        "x\"y",
    ];
    let ops = ["eq", "ne", "gt", "lt", "in", "nin", "like", "null", "zzz"];
    if depth < 3 && r.next() % 3 == 0 {
        return match r.next() % 3 {
            0 => json!({"and": [random_filter(r, depth + 1), random_filter(r, depth + 1)]}),
            1 => json!({"or": [random_filter(r, depth + 1), random_filter(r, depth + 1)]}),
            _ => json!({"not": random_filter(r, depth + 1)}),
        };
    }
    let values = [
        json!("requested"),
        json!("a' OR '1'='1"),
        json!(5),
        json!(["requested", "x"]),
        json!(null),
        json!("2026-01-01"),
    ];
    json!({"field": r.pick(&fields), "op": r.pick(&ops), "value": r.pick(&values)})
}

#[test]
fn fuzz_every_planned_statement_is_tenant_scoped() {
    let m = manifest("booking_crm");
    let mut r = Rng(0x9E3779B97F4A7C15);
    let (mut ok, mut rejected) = (0, 0);
    for _ in 0..5000 {
        let req = ListRequest {
            filter: Some(random_filter(&mut r, 0)),
            ..Default::default()
        };
        match plan(&m, "booking", &req) {
            Ok(p) => {
                ok += 1;
                assert_tenant_scoped(&p);
                assert!(
                    !p.sql.contains("1'='1"),
                    "value spliced into SQL: {}",
                    p.sql
                );
            }
            Err(_) => rejected += 1,
        }
    }
    assert!(
        ok > 100 && rejected > 100,
        "fuzz should hit both paths: ok={ok} rejected={rejected}"
    );
}
