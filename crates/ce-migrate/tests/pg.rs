//! Applies the generated DDL to a real Postgres and runs the query planner's SQL against it.
//! Set `CE_TEST_DATABASE_URL`. In CI also set `CE_REQUIRE_DB=1` so a missing database fails
//! the test instead of silently skipping it.
use ce_manifest::{validate, Manifest};
use ce_migrate::generate;
use ce_query::{plan_list, ListRequest, Param, SortDir, SortKey, Target, TenantScope};
use postgres::types::ToSql;
use postgres::{Client, NoTls};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";

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

fn schema_name() -> String {
    format!(
        "ce_mig_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn example(name: &str) -> Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    validate(std::fs::read_to_string(path).unwrap().as_bytes()).expect("example is valid")
}

/// Builds a scope the only legitimate way: by verifying a signed token.
fn scope(tenant: &str) -> TenantScope {
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

fn install(client: &mut Client, m: &Manifest, schema: &str) {
    let mut tx = client.transaction().unwrap();
    for stmt in generate(m, schema).unwrap() {
        tx.batch_execute(&stmt)
            .unwrap_or_else(|e| panic!("{e}\n--- failed statement ---\n{stmt}"));
    }
    tx.commit().unwrap();
}

fn drop_schema(client: &mut Client, schema: &str) {
    client
        .batch_execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .unwrap();
}

fn run(
    client: &mut Client,
    m: &Manifest,
    schema: &str,
    tenant: &str,
    entity: &str,
    req: &ListRequest,
) -> Vec<postgres::Row> {
    let planned = plan_list(
        m,
        &Target {
            schema: schema.into(),
            entity: entity.into(),
        },
        &scope(tenant),
        req,
    )
    .unwrap();
    let params: Vec<Box<dyn ToSql + Sync>> = planned
        .params
        .iter()
        .map(|p| match p {
            Param::Text(s) | Param::Dec(s) => Box::new(s.clone()) as Box<dyn ToSql + Sync>,
            Param::Int(i) => Box::new(*i),
            Param::Bool(b) => Box::new(*b),
        })
        .collect();
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
    client
        .query(planned.sql.as_str(), &refs)
        .unwrap_or_else(|e| panic!("{e}\n--- planned sql ---\n{}", planned.sql))
}

#[test]
fn all_examples_install_and_installing_twice_fails() {
    let Some(url) = url() else { return };
    let mut c = Client::connect(&url, NoTls).unwrap();
    for name in ["inventory", "booking_crm", "approval_workflow"] {
        let (m, schema) = (example(name), schema_name());
        install(&mut c, &m, &schema);
        let again = c.batch_execute(&generate(&m, &schema).unwrap()[0]);
        assert!(again.is_err(), "{name}: second install must fail loudly");
        drop_schema(&mut c, &schema);
    }
}

#[test]
fn planner_sql_runs_and_isolates_tenants() {
    let Some(url) = url() else { return };
    let mut c = Client::connect(&url, NoTls).unwrap();
    let (m, schema) = (example("inventory"), schema_name());
    install(&mut c, &m, &schema);

    let period_a = "aaaaaaaa-0000-0000-0000-000000000001";
    let period_b = "bbbbbbbb-0000-0000-0000-000000000001";
    for (id, tenant) in [(period_a, A), (period_b, B)] {
        insert_period(&mut c, &m, &schema, id, tenant);
    }

    for (i, (name, cost)) in [("Rice", "1.5000"), ("Beans", "2.0000"), ("Oil", "9.2500")]
        .iter()
        .enumerate()
    {
        c.execute(
            format!(
                "INSERT INTO \"{schema}\".\"item\" (\"id\", \"tenant_id\", \"created_at\", \"updated_at\", \"name\", \"unit_cost\", \"period\") \
                 VALUES ($1::text::uuid, $2::text::uuid, now(), now(), $3, $4::text::numeric, $5::text::uuid)"
            )
            .as_str(),
            &[&format!("cccccccc-0000-0000-0000-00000000000{}", i + 1), &A, name, cost, &period_a],
        )
        .unwrap();
    }
    c.execute(
        format!(
            "INSERT INTO \"{schema}\".\"item\" (\"id\", \"tenant_id\", \"created_at\", \"updated_at\", \"name\", \"unit_cost\") \
             VALUES ('dddddddd-0000-0000-0000-000000000001', $1::text::uuid, now(), now(), 'Other tenant', 1)"
        )
        .as_str(),
        &[&B],
    )
    .unwrap();

    // Sorted by name, tenant A only.
    let req = ListRequest {
        sort: vec![SortKey {
            field: "name".into(),
            dir: SortDir::Asc,
        }],
        ..Default::default()
    };
    let rows = run(&mut c, &m, &schema, A, "item", &req);
    let names: Vec<String> = rows.iter().map(|r| r.get("name")).collect();
    assert_eq!(names, ["Beans", "Oil", "Rice"]);

    // Tenant B sees only its own row.
    let rows = run(&mut c, &m, &schema, B, "item", &req);
    assert_eq!(rows.len(), 1);

    // Decimal filter, with the filter inside its own parentheses beside the tenant predicate.
    let req = ListRequest {
        filter: Some(json!({ "or": [
            { "field": "unit_cost", "op": "gte", "value": "2.0000" },
            { "field": "name", "op": "eq", "value": "nothing" }
        ]})),
        sort: vec![SortKey {
            field: "name".into(),
            dir: SortDir::Asc,
        }],
        ..Default::default()
    };
    let rows = run(&mut c, &m, &schema, A, "item", &req);
    let names: Vec<String> = rows.iter().map(|r| r.get("name")).collect();
    assert_eq!(names, ["Beans", "Oil"]);

    // Keyset page: after "Beans" (with its id) comes "Oil".
    let first = run(
        &mut c,
        &m,
        &schema,
        A,
        "item",
        &ListRequest {
            sort: vec![SortKey {
                field: "name".into(),
                dir: SortDir::Asc,
            }],
            limit: Some(1),
            ..Default::default()
        },
    );
    // limit + 1 rows come back; the extra one proves another page exists.
    assert_eq!(first.len(), 2);
    let id: uuid_text::Text = first[0].get("id");
    let after = vec![json!(first[0].get::<_, String>("name")), json!(id.0)];
    let next = run(
        &mut c,
        &m,
        &schema,
        A,
        "item",
        &ListRequest {
            sort: vec![SortKey {
                field: "name".into(),
                dir: SortDir::Asc,
            }],
            limit: Some(1),
            after: Some(after),
            ..Default::default()
        },
    );
    assert_eq!(next[0].get::<_, String>("name"), "Oil");

    drop_schema(&mut c, &schema);
}

#[test]
fn a_record_cannot_reference_another_tenants_record() {
    let Some(url) = url() else { return };
    let mut c = Client::connect(&url, NoTls).unwrap();
    let (m, schema) = (example("inventory"), schema_name());
    install(&mut c, &m, &schema);
    let period_b = "bbbbbbbb-0000-0000-0000-000000000001";
    insert_period(&mut c, &m, &schema, period_b, B);

    let res = c.execute(
        format!(
            "INSERT INTO \"{schema}\".\"item\" (\"id\", \"tenant_id\", \"created_at\", \"updated_at\", \"name\", \"unit_cost\", \"period\") \
             VALUES ('cccccccc-0000-0000-0000-000000000009', $1::text::uuid, now(), now(), 'x', 1, $2::text::uuid)"
        )
        .as_str(),
        &[&A, &period_b],
    );
    let err = res.expect_err("cross-tenant reference must be rejected");
    assert_eq!(err.code().map(|c| c.code()), Some("23503"), "{err}"); // foreign_key_violation
    drop_schema(&mut c, &schema);
}

/// `uuid` columns read back as text by casting in SQL would change the planner's output, so
/// the tests read them through this small wrapper instead.
mod uuid_text {
    use postgres::types::{FromSql, Type};
    pub struct Text(pub String);
    impl<'a> FromSql<'a> for Text {
        fn from_sql(
            _: &Type,
            raw: &'a [u8],
        ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
            let h: String = raw.iter().map(|b| format!("{b:02x}")).collect();
            Ok(Text(format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            )))
        }
        fn accepts(ty: &Type) -> bool {
            *ty == Type::UUID
        }
    }
}

fn insert_period(c: &mut Client, m: &Manifest, schema: &str, id: &str, tenant: &str) {
    // Fill every required field of `period` with a value valid for its type.
    let fields = m.value["entities"]["period"]["fields"].as_object().unwrap();
    let mut cols = vec![
        "\"id\"",
        "\"tenant_id\"",
        "\"created_at\"",
        "\"updated_at\"",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    let mut vals = vec![
        "$1::text::uuid".to_string(),
        "$2::text::uuid".into(),
        "now()".into(),
        "now()".into(),
    ];
    if let Some(sf) = m.value["entities"]["period"]["state"]["field"].as_str() {
        if !fields.contains_key(sf) {
            cols.push(format!("\"{sf}\""));
            vals.push(format!(
                "'{}'",
                m.value["entities"]["period"]["state"]["initial"]
                    .as_str()
                    .unwrap()
            ));
        }
    }
    for (name, f) in fields {
        if f["required"] == true {
            cols.push(format!("\"{name}\""));
            vals.push(match f["type"].as_str().unwrap() {
                "date" => "'2026-01-01'::date".into(),
                "integer" | "decimal" => "1".into(),
                "boolean" => "true".into(),
                "enum" => format!("'{}'", f["values"][0].as_str().unwrap()),
                _ => "'x'".into(),
            });
        }
    }
    c.execute(
        format!(
            "INSERT INTO \"{schema}\".\"period\" ({}) VALUES ({})",
            cols.join(", "),
            vals.join(", ")
        )
        .as_str(),
        &[&id, &tenant],
    )
    .unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn two_tenants_may_use_the_same_record_id_and_big_limits_install() {
    let Some(url) = url() else { return };
    let mut c = Client::connect(&url, NoTls).unwrap();
    let mut v = example("inventory").value.clone();
    v["entities"]["item"]["fields"]["name"]["max_length"] = json!(10_485_761);
    let m = validate(v.to_string().as_bytes()).expect("still valid");
    let schema = schema_name();
    install(&mut c, &m, &schema);
    let id = "aaaaaaaa-0000-0000-0000-000000000001";
    insert_period(&mut c, &m, &schema, id, A);
    insert_period(&mut c, &m, &schema, id, B);
    // and an oversized value is still refused by the CHECK
    insert_period(
        &mut c,
        &m,
        &schema,
        "aaaaaaaa-0000-0000-0000-000000000002",
        A,
    );
    let long = "x".repeat(10_485_762);
    let res = c.execute(
        format!(
            "INSERT INTO \"{schema}\".\"item\" (\"id\", \"tenant_id\", \"created_at\", \"updated_at\", \"name\", \"unit_cost\") \
             VALUES ('cccccccc-0000-0000-0000-000000000001', $1::text::uuid, now(), now(), $2, 1)"
        )
        .as_str(),
        &[&A, &long],
    );
    assert_eq!(
        res.expect_err("too long").code().map(|c| c.code()),
        Some("23514")
    );
    drop_schema(&mut c, &schema);
}
