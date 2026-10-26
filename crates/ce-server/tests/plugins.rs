//! One module, two tenant-private plugins, no engine change: the same inventory module gives
//! different results under different policy packs, end to end through the HTTP API.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::Verifier;
use ce_manifest::validate_composed;
use ce_server::{router, AppState};
use ce_store_pg::TypedPgStore;
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::Sha256;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const T1: &str = "11111111-1111-1111-1111-111111111111";
const INST: &str = "plug-1";

fn token(tenant: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"{tenant}","roles":["owner"],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

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

fn build(url: &str, m: Value) -> Router {
    let m = validate_composed(m.to_string().as_bytes()).unwrap_or_else(|e| panic!("{e:?}"));
    let schema = format!(
        "ce_plug_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let store = TypedPgStore::install(url, &m, &schema).unwrap();
    let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
    st.now = std::sync::Arc::new(|| 1000);
    router(st)
}

async fn app(url: &str, m: Value) -> Router {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || build(&url, m))
        .await
        .unwrap()
}

async fn close(a: Router) {
    tokio::task::spawn_blocking(move || drop(a)).await.unwrap();
}

struct Api {
    app: Router,
    n: u32,
}

impl Api {
    async fn call(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.n += 1;
        let key = format!("k{}", self.n);
        self.call_keyed(method, uri, version, body, &key).await
    }

    async fn call_keyed(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
        key: &str,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", token(T1)))
            .header("idempotency-key", key);
        if let Some(v) = version {
            req = req.header("if-match", format!("\"{v}\""));
        }
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let res = self
            .app
            .clone()
            .oneshot(
                req.body(match body {
                    Some(b) => Body::from(b.to_string()),
                    None => Body::empty(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn post(&mut self, entity: &str, body: Value) -> (StatusCode, Value) {
        self.call("POST", &format!("/v1/{INST}/{entity}"), None, Some(body))
            .await
    }
    async fn get(&mut self, entity: &str, id: &str) -> Value {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/{entity}/{id}"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }
}

impl Api {
    async fn patch(
        &mut self,
        entity: &str,
        id: &str,
        version: u64,
        body: Value,
    ) -> (StatusCode, Value) {
        self.call(
            "PATCH",
            &format!("/v1/{INST}/{entity}/{id}"),
            Some(version),
            Some(body),
        )
        .await
    }
    async fn delete(&mut self, entity: &str, id: &str, version: u64) -> StatusCode {
        self.call(
            "DELETE",
            &format!("/v1/{INST}/{entity}/{id}"),
            Some(version),
            None,
        )
        .await
        .0
    }
}

fn plugin(id: &str, slots: &str) -> ce_plugin::Plugin {
    let text = format!(
        r#"schema_version = 1
[plugin]
id = "{id}"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
{slots}"#
    );
    ce_plugin::parse(
        &text,
        &ce_plugin::builtin_registry(),
        &semver::Version::new(0, 1, 0),
    )
    .unwrap_or_else(|e| panic!("{e:?}"))
}

const DIGEST: &str = "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn manifest(p: Option<&ce_plugin::Plugin>) -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    let shell = ce_tools::compose(&shell, &["period", "audit"]).unwrap();
    let pinned = p.map(|p| (p, DIGEST));
    ce_plugin::compose(&shell, "business.inventory", pinned)
        .unwrap()
        .manifest
}

fn id(v: &Value) -> String {
    v["data"]["id"].as_str().unwrap().to_string()
}

/// Same requests, whatever the policy: a priced item, an open period, a count of 10, and a
/// pull of 12 that overdraws it.
async fn run(url: &str, m: Value) -> (Value, (StatusCode, Value)) {
    let mut api = Api {
        app: app(url, m).await,
        n: 0,
    };
    let (s, period) = api
        .post(
            "period",
            json!({"label": "2026-W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{period}");
    let (s, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "Rice 25lb", "unit_price": "1.234568"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{item}");
    let (s, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&period), "opening_oh": "10", "unit_price": "1.234568"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{line}");
    let lid = id(&line);
    let pid = id(&period);
    let pull = api
        .post(
            "stock_movement",
            json!({"line": lid, "period": pid, "pulled": "12"}),
        )
        .await;
    let cur = api.get("stock_line", &lid).await;
    close(api.app).await;
    (cur, pull)
}

#[tokio::test]
async fn one_module_two_plugins_two_outcomes() {
    let Some(url) = url() else { return };

    // Policy A is the module's defaults: price kept to six places, stock never below zero,
    // and a pull that overdraws a line is refused.
    let (line, pull) = run(&url, manifest(None)).await;
    assert_eq!(pull.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", pull.1);
    assert_eq!(line["data"]["price"], "1.234568");
    assert_eq!(line["data"]["ending_qty"], "10.000");
    assert_eq!(line["data"]["ending_value"], "12.35");

    // Policy B is a different plugin on the same module: price to four places, negative stock
    // allowed and shown as it is. No Rust changed between the two runs.
    let b = plugin(
        "lenient-rules",
        r#"[[policy]]
slot = "price"
rule = "round(max(0, unit_price), 4)"
[[policy]]
slot = "ending_qty"
rule = "opening_oh + received - pulled"
[[policy]]
slot = "over_pull"
rule = "true"
"#,
    );
    let (line, pull) = run(&url, manifest(Some(&b))).await;
    assert_eq!(pull.0, StatusCode::CREATED, "{}", pull.1);
    assert_eq!(line["data"]["price"], "1.234600");
    assert_eq!(line["data"]["ending_qty"], "-2.000");
    assert_eq!(line["data"]["ending_value"], "-2.47");
}

#[tokio::test]
async fn a_closed_period_freezes_its_lines_and_movements() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest(None)).await,
        n: 0,
    };
    let (_, p1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, p2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    let (_, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "Rice", "unit_price": "2"}),
        )
        .await;
    let mk_line = |p: &Value| json!({"item": id(&item), "period": id(p), "opening_oh": "10", "unit_price": "2"});
    let (_, l1) = api.post("stock_line", mk_line(&p1)).await;
    let (_, l2) = api.post("stock_line", mk_line(&p2)).await;
    let (s, m1) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "3"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{m1}");
    assert_eq!(
        api.get("stock_line", &id(&l1)).await["data"]["ending_qty"],
        "7.000"
    );
    assert_eq!(
        api.get("stock_line", &id(&l1)).await["data"]["ending_value"],
        "14.00"
    );

    // The item's catalog price changes; the line keeps the price stamped on it.
    let cur = api.get("stock_item", &id(&item)).await;
    let (s, r) = api
        .patch(
            "stock_item",
            &id(&item),
            cur["data"]["version"].as_u64().unwrap(),
            json!({"unit_price": "5"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (s, m3) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{m3}");
    let line = api.get("stock_line", &id(&l1)).await;
    assert_eq!(
        (&line["data"]["ending_qty"], &line["data"]["ending_value"]),
        (&json!("6.000"), &json!("12.00"))
    );

    // A movement must belong to its line's period.
    let (s, bad) = api
        .post(
            "stock_movement",
            json!({"line": id(&l2), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");

    // Close period 1.
    let (s, closed) = api
        .call(
            "POST",
            &format!("/v1/{INST}/period/{}/close", id(&p1)),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{closed}");

    // Nothing in it can change: not the line, and not a movement added, edited or removed.
    let (s, r) = api
        .post(
            "stock_movement",
            json!({"line": id(&l1), "period": id(&p1), "pulled": "1"}),
        )
        .await;
    assert_ne!(s, StatusCode::CREATED, "{r}");
    let (s, r) = api
        .patch("stock_movement", &id(&m1), 1, json!({"pulled": "9"}))
        .await;
    assert_ne!(s, StatusCode::OK, "{r}");
    assert_ne!(
        api.delete("stock_movement", &id(&m1), 1).await,
        StatusCode::NO_CONTENT
    );
    // A movement of the open period cannot be pointed at the closed one either.
    let (_, m2) = api
        .post(
            "stock_movement",
            json!({"line": id(&l2), "period": id(&p2), "pulled": "1"}),
        )
        .await;
    let (s, r) = api
        .patch(
            "stock_movement",
            &id(&m2),
            1,
            json!({"line": id(&l1), "period": id(&p1)}),
        )
        .await;
    assert_ne!(s, StatusCode::OK, "{r}");
    let l1 = api.get("stock_line", &id(&l1)).await;
    assert_eq!(
        (&l1["data"]["pulled"], &l1["data"]["ending_qty"]),
        (&json!("4.000"), &json!("6.000"))
    );
    close(api.app).await;
}

/// A locked week freezes the movements tagged with it; other weeks and untagged movements move on.
#[tokio::test]
async fn a_locked_week_freezes_its_movements_only() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest(None)).await,
        n: 0,
    };
    let (_, p) = api
        .post(
            "period",
            json!({"label": "Oct", "starts_on": "2026-10-01", "ends_on": "2026-10-31"}),
        )
        .await;
    let (_, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "Rice", "unit_price": "2"}),
        )
        .await;
    let (_, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&p), "opening_oh": "10", "unit_price": "2"}),
        )
        .await;
    let mv = |week: Option<u32>| {
        let mut v = json!({"line": id(&line), "period": id(&p), "pulled": "1"});
        if let Some(w) = week {
            v["week"] = json!(w);
        }
        v
    };
    let (s, w1) = api.post("stock_movement", mv(Some(1))).await;
    assert_eq!(s, StatusCode::CREATED, "{w1}");
    assert_eq!(
        api.post("stock_movement", mv(Some(5))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let cur = api.get("period", &id(&p)).await;
    let (s, r) = api
        .patch(
            "period",
            &id(&p),
            cur["data"]["version"].as_u64().unwrap(),
            json!({"week_1_locked": true}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    let (s, r) = api.post("stock_movement", mv(Some(1))).await;
    assert_ne!(s, StatusCode::CREATED, "{r}");
    let (s, r) = api
        .patch("stock_movement", &id(&w1), 1, json!({"pulled": "9"}))
        .await;
    assert_ne!(s, StatusCode::OK, "{r}");
    assert_ne!(
        api.delete("stock_movement", &id(&w1), 1).await,
        StatusCode::NO_CONTENT
    );
    // Week 2 and an untagged movement are untouched.
    let (s, w2) = api.post("stock_movement", mv(Some(2))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, loose) = api.post("stock_movement", mv(None)).await;
    assert_eq!(s, StatusCode::CREATED);
    // Moving a movement into the locked week is refused, tagged or not, and changes nothing.
    for m in [&w2, &loose] {
        let (s, r) = api
            .patch(
                "stock_movement",
                &id(m),
                1,
                json!({"week": 1, "pulled": "9"}),
            )
            .await;
        assert_eq!(s, StatusCode::CONFLICT, "{r}");
    }
    let (s, _) = api
        .patch("stock_movement", &id(&w2), 1, json!({"week": 3}))
        .await;
    assert_eq!(s, StatusCode::OK);
    // Unlocking reopens week 1.
    let cur = api.get("period", &id(&p)).await;
    api.patch(
        "period",
        &id(&p),
        cur["data"]["version"].as_u64().unwrap(),
        json!({"week_1_locked": false}),
    )
    .await;
    assert_eq!(
        api.post("stock_movement", mv(Some(1))).await.0,
        StatusCode::CREATED
    );
    close(api.app).await;
}

#[tokio::test]
async fn every_audit_event_names_the_plugin_that_ruled() {
    let Some(url) = url() else { return };
    let b = plugin(
        "lenient-rules",
        "[[policy]]\nslot = \"ending_qty\"\nrule = \"opening_oh + received - pulled\"\n[[policy]]\nslot = \"over_pull\"\nrule = \"true\"\n",
    );
    let mut api = Api {
        app: app(&url, manifest(Some(&b))).await,
        n: 0,
    };
    let (s, _) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, v) = api
        .call("GET", &format!("/v1/{INST}/audit_event"), None, None)
        .await;
    let events = v["data"].as_array().unwrap();
    assert!(!events.is_empty(), "{v}");
    for e in events {
        let d = e;
        assert_eq!(d["provenance"]["plugin"]["id"], "lenient-rules", "{e}");
        assert_eq!(d["provenance"]["plugin"]["digest"], DIGEST, "{e}");
        assert_eq!(d["provenance"]["module"]["name"], "business.inventory");
        assert_eq!(d["manifest_hash"].as_str().unwrap().len(), 64);
    }
    close(api.app).await;
}

/// Rollover is a client procedure over the batch endpoint: read the closed period's ending
/// counts, write the next period's opening lines in chunks, each under its own idempotency key.
/// Re-running a chunk after a lost response creates nothing twice.
#[tokio::test]
async fn rollover_carries_each_ending_into_the_next_opening_and_is_safe_to_rerun() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest(None)).await,
        n: 0,
    };
    let (_, w1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, w2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    let (w1, w2) = (id(&w1), id(&w2));
    let mut expect = Vec::new();
    for (i, (open, pull)) in [("10", "3"), ("5", "5"), ("8", "1"), ("2", "0"), ("7", "2")]
        .into_iter()
        .enumerate()
    {
        let (_, item) = api
            .post(
                "stock_item",
                json!({"sku": format!("S{i}"), "description": "x", "unit_price": "1.5"}),
            )
            .await;
        let (s, line) = api
            .post(
                "stock_line",
                json!({"item": id(&item), "period": w1, "opening_oh": open, "unit_price": "1.5"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{line}");
        let (s, m) = api
            .post(
                "stock_movement",
                json!({"line": id(&line), "period": w1, "pulled": pull}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{m}");
        expect.push((id(&item), api.get("stock_line", &id(&line)).await));
    }
    let (s, v) = api
        .call("POST", &format!("/v1/{INST}/period/{w1}/close"), None, None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // Chunks of two, as a client with a batch limit would send them.
    let ops: Vec<Value> = expect
        .iter()
        .map(|(item, line)| {
            json!({"op": "create", "entity": "stock_line", "data": {
                "item": item, "period": w2,
                "opening_oh": line["data"]["ending_qty"],
                "unit_price": line["data"]["unit_price"],
            }})
        })
        .collect();
    for pass in 0..2 {
        for (n, chunk) in ops.chunks(2).enumerate() {
            let (s, v) = api
                .call_keyed(
                    "POST",
                    &format!("/v1/{INST}/_batch"),
                    None,
                    Some(json!({"ops": chunk})),
                    &format!("rollover-w2-{n}"),
                )
                .await;
            assert_eq!(s, StatusCode::OK, "pass {pass}: {v}");
        }
    }
    let (_, all) = api
        .call(
            "GET",
            &format!("/v1/{INST}/stock_line?limit=100"),
            None,
            None,
        )
        .await;
    let next: Vec<&Value> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["period"] == w2.as_str())
        .collect();
    assert_eq!(next.len(), 5, "a rerun must not duplicate lines");
    for (item, prev) in &expect {
        let l = next.iter().find(|l| l["item"] == item.as_str()).unwrap();
        assert_eq!(l["opening_oh"], prev["data"]["ending_qty"]);
        assert_eq!(l["price"], prev["data"]["price"]);
    }
    // A second run under a new key conflicts on the unique (item, period) line_key instead of
    // duplicating.
    let (s, v) = api
        .call_keyed(
            "POST",
            &format!("/v1/{INST}/_batch"),
            None,
            Some(json!({"ops": ops[..1]})),
            "rollover-w2-again",
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // The closed period is untouched.
    let first = api.get("stock_line", &id(&expect[0].1)).await;
    assert_eq!(
        first["data"]["ending_qty"],
        expect[0].1["data"]["ending_qty"]
    );
    close(api.app).await;
}

/// A plugin that lets stock go negative cannot roll the shortfall forward: `opening_oh` is never
/// negative, so the batch is refused whole. The client settles the shortfall first.
#[tokio::test]
async fn a_negative_ending_cannot_be_carried_as_an_opening() {
    let Some(url) = url() else { return };
    let lenient = plugin(
        "lenient-rules",
        "[[policy]]\nslot = \"ending_qty\"\nrule = \"opening_oh + received - pulled\"\n[[policy]]\nslot = \"over_pull\"\nrule = \"true\"\n",
    );
    let mut api = Api {
        app: app(&url, manifest(Some(&lenient))).await,
        n: 0,
    };
    let (_, w1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, w2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    let (_, item) = api
        .post(
            "stock_item",
            json!({"sku": "RICE", "description": "x", "unit_price": "1.5"}),
        )
        .await;
    let (_, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&w1), "opening_oh": "10", "unit_price": "1.5"}),
        )
        .await;
    let (s, _) = api
        .post(
            "stock_movement",
            json!({"line": id(&line), "period": id(&w1), "pulled": "12"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let cur = api.get("stock_line", &id(&line)).await;
    assert_eq!(cur["data"]["ending_qty"], "-2.000");
    let (s, v) = api
        .call(
            "POST",
            &format!("/v1/{INST}/_batch"),
            None,
            Some(
                json!({"ops": [{"op": "create", "entity": "stock_line", "data": {
                "item": id(&item), "period": id(&w2),
                "opening_oh": cur["data"]["ending_qty"], "unit_price": "1.5"}}]}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    close(api.app).await;
}

impl Api {
    async fn act(
        &mut self,
        name: &str,
        id: &str,
        version: u64,
        input: Value,
    ) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/actions/{name}"),
            Some(version),
            Some(json!({"id": id, "input": input})),
        )
        .await
    }
}

/// A fresh item and line (price `current`) in the period; returns the line id.
async fn priced_line(api: &mut Api, period: &str, sku: &str, current: &str) -> String {
    let (s, item) = api
        .post(
            "stock_item",
            json!({"sku": sku, "description": sku, "unit_price": current}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{item}");
    let (s, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": period, "opening_oh": "10", "unit_price": current}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{line}");
    id(&line)
}

/// Records an observation and settles it; returns the settled observation and the line's price.
async fn observe(api: &mut Api, line: &str, observed: &str) -> (Value, Value) {
    let (s, o) = api
        .post(
            "price_observation",
            json!({"line": line, "observed_price": observed, "source": "inv-1"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{o}");
    let (s, done) = api
        .act("settle_price_observation", &id(&o), 1, json!({}))
        .await;
    assert_eq!(s, StatusCode::OK, "{done}");
    let price = api.get("stock_line", line).await["data"]["unit_price"].clone();
    (done, price)
}

#[tokio::test]
async fn an_observed_price_is_applied_or_held_for_review_by_the_inventory_rule() {
    let Some(url) = url() else { return };
    let mut api = Api {
        app: app(&url, manifest(None)).await,
        n: 0,
    };
    let (_, period) = api
        .post(
            "period",
            json!({"label": "2026-W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let pid = id(&period);

    // Exactly 25% up applies at once; the line takes the new price.
    let l1 = priced_line(&mut api, &pid, "A", "40.000000").await;
    let (o, price) = observe(&mut api, &l1, "50.000000").await;
    assert_eq!(o["data"]["status"], "auto_applied");
    assert_eq!(o["data"]["decision"], "apply");
    assert_eq!(o["data"]["previous_price"], "40.000000");
    assert_eq!(o["data"]["change"], "0.250000");
    assert_eq!(price, "50.000000");

    // A hair over 25% is held: the line keeps its price until a manager decides.
    let l2 = priced_line(&mut api, &pid, "B", "40.000000").await;
    let (o, price) = observe(&mut api, &l2, "50.010000").await;
    assert_eq!(o["data"]["status"], "pending");
    assert_eq!(o["data"]["decision"], "hold");
    assert_eq!(price, "40.000000");
    let oid = id(&o);
    let v = o["data"]["version"].as_u64().unwrap();

    // The manager applies it with their own price.
    let (s, done) = api
        .act("apply_price_observation", &oid, v, json!({"price": "45"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{done}");
    assert_eq!(done["data"]["status"], "applied");
    assert_eq!(done["data"]["applied_price"], "45.000000");
    assert_eq!(
        api.get("stock_line", &l2).await["data"]["unit_price"],
        "45.000000"
    );
    // A decided observation cannot be decided again.
    let v = done["data"]["version"].as_u64().unwrap();
    let (s, _) = api.act("keep_price_observation", &oid, v, json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Kept: the price stays.
    let l3 = priced_line(&mut api, &pid, "C", "40.000000").await;
    let (o, _) = observe(&mut api, &l3, "10.000000").await;
    assert_eq!(o["data"]["status"], "pending");
    let (s, kept) = api
        .act(
            "keep_price_observation",
            &id(&o),
            o["data"]["version"].as_u64().unwrap(),
            json!({}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{kept}");
    assert_eq!(kept["data"]["status"], "kept");
    assert_eq!(
        api.get("stock_line", &l3).await["data"]["unit_price"],
        "40.000000"
    );

    // Within half a cent is the same price; a zero price is no usable price. Neither changes the line.
    let l4 = priced_line(&mut api, &pid, "D", "40.000000").await;
    let (o, price) = observe(&mut api, &l4, "40.004000").await;
    assert_eq!(
        (o["data"]["decision"].as_str(), price.as_str()),
        (Some("same"), Some("40.000000"))
    );
    assert_eq!(o["data"]["status"], "dismissed");
    let (o, price) = observe(&mut api, &l4, "0").await;
    assert_eq!(
        (o["data"]["decision"].as_str(), price.as_str()),
        (Some("skip"), Some("40.000000"))
    );

    // A line with no price yet takes the first observed one.
    let l5 = priced_line(&mut api, &pid, "E", "0").await;
    let (o, price) = observe(&mut api, &l5, "3.500000").await;
    assert_eq!(o["data"]["decision"], "apply");
    assert_eq!(price, "3.500000");

    // The widest accepted move still settles (held), instead of overflowing `change`.
    let l5b = priced_line(&mut api, &pid, "E2", "0.000001").await;
    let (o, price) = observe(&mut api, &l5b, "999999.999999").await;
    assert_eq!(o["data"]["decision"], "hold");
    assert_eq!(price, "0.000001");

    // Once the period is closed the line is frozen: the observation cannot move its price, and
    // stays unsettled.
    let l6 = priced_line(&mut api, &pid, "F", "40.000000").await;
    let (_, o) = api
        .post(
            "price_observation",
            json!({"line": l6, "observed_price": "41.000000", "source": "inv-9"}),
        )
        .await;
    let (s, closed) = api
        .call(
            "POST",
            &format!("/v1/{INST}/period/{pid}/close"),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{closed}");
    let (s, v) = api
        .act("settle_price_observation", &id(&o), 1, json!({}))
        .await;
    assert!(!s.is_success(), "{s} {v}");
    let after = api.get("price_observation", &id(&o)).await;
    assert_eq!(after["data"]["status"], "new");
    assert_eq!(
        api.get("stock_line", &l6).await["data"]["unit_price"],
        "40.000000"
    );
    close(api.app).await;
}

/// The legacy MJCC over-pull rule: stock may not go below zero, but a line that is already short
/// stays editable as long as the shortfall does not grow.
const GRANDFATHER: &str = r#"[[policy]]
slot = "ending_qty"
rule = "opening_oh + received - pulled"
[[policy]]
slot = "over_pull"
rule = "opening_oh + received - pulled >= 0 or opening_oh + received - pulled >= coalesce(old.opening_oh, 0) + coalesce(old.received, 0) - coalesce(old.pulled, 0)"
"#;

#[tokio::test]
async fn an_existing_shortfall_may_shrink_but_never_grow_under_the_legacy_over_pull_rule() {
    let Some(url) = url() else { return };
    let schema = format!(
        "ce_plug_gf_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    );
    let open = |m: Value, first: bool| {
        let url = url.clone();
        let schema = schema.clone();
        tokio::task::spawn_blocking(move || {
            let m = validate_composed(m.to_string().as_bytes()).unwrap();
            // The second app reads the schema the first one installed.
            let store = if first {
                TypedPgStore::install(&url, &m, &schema).unwrap()
            } else {
                TypedPgStore::open(&url, &m, &schema).unwrap()
            };
            let mut st = AppState::new(store, Verifier::new(KEY).unwrap(), m, INST);
            st.now = std::sync::Arc::new(|| 1000);
            router(st)
        })
    };
    // History written under a permissive policy: 10 on hand, 12 pulled.
    let lenient = plugin(
        "lenient",
        "[[policy]]\nslot = \"ending_qty\"\nrule = \"opening_oh + received - pulled\"\n[[policy]]\nslot = \"over_pull\"\nrule = \"true\"\n",
    );
    let mut api = Api {
        app: open(manifest(Some(&lenient)), true).await.unwrap(),
        n: 0,
    };
    let (_, period) = api
        .post(
            "period",
            json!({"label": "2026-W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, item) = api
        .post(
            "stock_item",
            json!({"sku": "X", "description": "x", "unit_price": "1"}),
        )
        .await;
    let (_, line) = api
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&period), "opening_oh": "10", "unit_price": "1"}),
        )
        .await;
    let (s, mv) = api
        .post(
            "stock_movement",
            json!({"line": id(&line), "period": id(&period), "pulled": "12"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{mv}");
    let (mid, mut ver) = (id(&mv), mv["data"]["version"].as_u64().unwrap());
    let lid = id(&line);
    assert_eq!(
        api.get("stock_line", &lid).await["data"]["ending_qty"],
        "-2.000"
    );
    let old_app = api.app.clone();
    drop(api);

    // The same data under the MJCC rule.
    let rule = plugin("legacy-overpull", GRANDFATHER);
    let mut api = Api {
        app: open(manifest(Some(&rule)), false).await.unwrap(),
        n: 100,
    };
    // Growing the shortfall (12 -> 13) is refused.
    let (s, v) = api
        .patch("stock_movement", &mid, ver, json!({"pulled": "13"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // Shrinking it (12 -> 11) and leaving it alone are both allowed.
    let (s, v) = api
        .patch("stock_movement", &mid, ver, json!({"pulled": "11"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    ver = v["data"]["version"].as_u64().unwrap();
    assert_eq!(
        api.get("stock_line", &lid).await["data"]["ending_qty"],
        "-1.000"
    );
    let (s, v) = api
        .patch("stock_movement", &mid, ver, json!({"reason": "recount"}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // A brand new over-pull is still refused.
    let (s, v) = api
        .post(
            "stock_movement",
            json!({"line": lid, "period": id(&period), "pulled": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    close(api.app).await;
    close(old_app).await;
}

/// MJCC's valuation rules (a synthetic copy; the real plugin lives in the customer repo).
const LEGACY_VALUATION: &str = r#"[[policy]]
slot = "opening_value"
rule = "when(opening_oh > 0, round(opening_oh * when(opening_unit_cost > 0, opening_unit_cost, price), 2), 0)"
[[policy]]
slot = "ending_value"
rule = "when(ending_qty <= 0, 0, round(max(0, opening_value + received_value - pulled_value), 2))"
"#;

#[tokio::test]
async fn rollover_carries_cost_so_a_new_price_does_not_revalue_what_was_on_hand() {
    let Some(url) = url() else { return };
    let rules = plugin("legacy-valuation", LEGACY_VALUATION);
    let mut api = Api {
        app: app(&url, manifest(Some(&rules))).await,
        n: 0,
    };
    let (_, w1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, w2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    // Rice: 7 at 1.428571, one pulled; oil: 2 at 5, both pulled.
    let mut closing = Vec::new();
    for (sku, open, price, pull) in [("RICE", "7", "1.428571", "1"), ("OIL", "2", "5", "2")] {
        let (_, item) = api
            .post(
                "stock_item",
                json!({"sku": sku, "description": sku, "unit_price": price}),
            )
            .await;
        let (_, line) = api
            .post(
                "stock_line",
                json!({"item": id(&item), "period": id(&w1), "opening_oh": open, "unit_price": price}),
            )
            .await;
        let (s, _) = api
            .post(
                "stock_movement",
                json!({"line": id(&line), "period": id(&w1), "pulled": pull}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        closing.push((
            id(&item),
            api.get("stock_line", &id(&line)).await["data"].clone(),
        ));
    }
    assert_eq!(closing[0].1["ending_qty"], "6.000");
    assert_eq!(closing[0].1["ending_value"], "8.57");
    assert_eq!(closing[1].1["ending_qty"], "0.000");
    assert_eq!(closing[1].1["ending_value"], "0.00");

    // The client derives each line's carried cost: closing value / closing quantity to six
    // places (1.428333, not exact), and nothing for a line with no stock. The supplier has
    // since raised both prices to 3.
    let ops: Vec<Value> = closing
        .iter()
        .map(|(item, c)| {
            let qty: f64 = c["ending_qty"].as_str().unwrap().parse().unwrap();
            let value: f64 = c["ending_value"].as_str().unwrap().parse().unwrap();
            let mut data = json!({"item": item, "period": id(&w2), "opening_oh": c["ending_qty"],
                "unit_price": "3"});
            if qty > 0.0 {
                data["opening_unit_cost"] = json!(format!("{:.6}", value / qty));
            }
            json!({"op": "create", "entity": "stock_line", "data": data})
        })
        .collect();
    assert_eq!(ops[0]["data"]["opening_unit_cost"], "1.428333");
    assert!(ops[1]["data"]["opening_unit_cost"].is_null());
    let (s, v) = api
        .call(
            "POST",
            &format!("/v1/{INST}/_batch"),
            None,
            Some(json!({ "ops": ops })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let rice = api
        .get("stock_line", v["data"][0]["id"].as_str().unwrap())
        .await;
    let oil = api
        .get("stock_line", v["data"][1]["id"].as_str().unwrap())
        .await;
    // 6 x 1.428333 = 8.569998, so the opening stays 8.57, not 6 x 3 = 18.00.
    assert_eq!(rice["data"]["opening_value"], "8.57");
    assert_eq!(rice["data"]["ending_value"], "8.57");
    assert_eq!(oil["data"]["opening_value"], "0.00");
    close(api.app).await;
}

/// The same rollover as the client procedure above, as one server action on the closed period:
/// it needs the period closed, copies every line with its carried cost, refuses a second run,
/// and leaves the closed period alone.
#[tokio::test]
async fn the_rollover_action_carries_quantity_and_cost_in_one_request() {
    let Some(url) = url() else { return };
    let rules = plugin("legacy-valuation", LEGACY_VALUATION);
    let mut api = Api {
        app: app(&url, manifest(Some(&rules))).await,
        n: 0,
    };
    let (_, w1) = api
        .post(
            "period",
            json!({"label": "W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, w2) = api
        .post(
            "period",
            json!({"label": "W2", "starts_on": "2026-11-08", "ends_on": "2026-11-14"}),
        )
        .await;
    let (w1id, w2id) = (id(&w1), id(&w2));
    for (sku, open, price, pull) in [("RICE", "7", "1.428571", "1"), ("OIL", "2", "5", "2")] {
        let (_, item) = api
            .post(
                "stock_item",
                json!({"sku": sku, "description": sku, "unit_price": price}),
            )
            .await;
        let (_, line) = api
            .post(
                "stock_line",
                json!({"item": id(&item), "period": w1id, "opening_oh": open, "unit_price": price, "par_level": if sku == "RICE" { 8 } else { 0 }}),
            )
            .await;
        let (s, _) = api
            .post(
                "stock_movement",
                json!({"line": id(&line), "period": w1id, "pulled": pull}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
    }
    async fn run(api: &mut Api, from: &str, to: &str, v: u64) -> (StatusCode, Value) {
        api.call(
            "POST",
            &format!("/v1/{INST}/actions/rollover_stock"),
            Some(v),
            Some(json!({"id": from, "input": {"to": to}})),
        )
        .await
    }
    // An open period has not finished moving, so it cannot be carried.
    let (s, v) = run(&mut api, &w1id, &w2id, 1).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = api
        .call(
            "POST",
            &format!("/v1/{INST}/period/{w1id}/close"),
            None,
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let version = v["data"]["version"].as_u64().unwrap();

    let (s, v) = run(&mut api, &w1id, &w2id, version).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, all) = api
        .call(
            "GET",
            &format!("/v1/{INST}/stock_line?limit=100"),
            None,
            None,
        )
        .await;
    let next: Vec<&Value> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["period"] == w2id.as_str())
        .collect();
    assert_eq!(next.len(), 2, "{all}");
    let rice = next.iter().find(|l| l["opening_oh"] == "6.000").unwrap();
    assert_eq!(rice["opening_unit_cost"], "1.428333");
    // The par the line was counted against rides along, so the reorder list survives the rollover.
    assert_eq!(rice["par_level"], 8, "{rice}");
    assert_eq!(rice["below_par"], true, "{rice}");
    let oil = next.iter().find(|l| l["opening_oh"] == "0.000").unwrap();
    assert_eq!(oil["opening_unit_cost"], "0.000000");
    // The supplier then raises the price to 3: the opening stock keeps its carried cost.
    let (s, p) = api
        .patch(
            "stock_line",
            rice["id"].as_str().unwrap(),
            rice["version"].as_u64().unwrap(),
            json!({"unit_price": "3"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["data"]["opening_value"], "8.57");

    // Running it again is refused whole and changes nothing: the period moved on with the first run.
    let (s, v) = run(&mut api, &w1id, &w2id, version).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (_, again) = api
        .call(
            "GET",
            &format!("/v1/{INST}/stock_line?limit=100"),
            None,
            None,
        )
        .await;
    assert_eq!(again["data"].as_array().unwrap().len(), 4);
    close(api.app).await;
}
