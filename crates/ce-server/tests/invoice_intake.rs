//! Invoice intake through Source Control: staff stage an invoice, its lines, the stock it
//! received and the prices it observed in one changeset; a manager merges it atomically and then
//! settles the price observations. A repeated invoice is refused whole.
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
const INST: &str = "inv-1";

fn token(tenant: &str, role: &str, sub: &str) -> String {
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"{sub}","tenant":"{tenant}","roles":["{role}"],"exp":2000}}"#
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
        "ce_inv_{}_{}",
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

/// Drops every handle to the app away from the async runtime: closing the database client
/// blocks, which is not allowed on a runtime thread.
async fn close(apps: Vec<Router>) {
    tokio::task::spawn_blocking(move || drop(apps))
        .await
        .unwrap();
}

struct Api {
    app: Router,
    n: u32,
    role: &'static str,
    extra: Option<String>,
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

    async fn call_with(
        &mut self,
        method: &str,
        uri: &str,
        version: Option<u64>,
        body: Option<Value>,
        changeset: &str,
    ) -> (StatusCode, Value) {
        self.extra = Some(changeset.to_string());
        let r = self.call(method, uri, version, body).await;
        self.extra = None;
        r
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
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", token(T1, self.role, self.role)),
            )
            .header("idempotency-key", key);
        if let Some(c) = &self.extra {
            req = req.header("x-ce-changeset", c.as_str());
        }
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
    async fn verb(&mut self, id: &str, verb: &str) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/changeset/{id}/{verb}"),
            None,
            None,
        )
        .await
    }
    async fn list(&mut self, entity: &str) -> Vec<Value> {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/{entity}?limit=100"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].as_array().unwrap().clone()
    }
}

async fn api(role: &'static str, app: Router) -> Api {
    Api {
        app,
        n: 0,
        role,
        extra: None,
    }
}

/// Same app, another caller.
fn as_role(a: &Api, role: &'static str) -> Api {
    Api {
        app: a.app.clone(),
        n: 1000,
        role,
        extra: None,
    }
}

fn manifest() -> Value {
    let shell = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "roles": {
            "owner": {"read": ["*"], "write": ["*"], "actions": ["*"]},
            "clerk": {"read": ["*"],
                "write": ["supplier_invoice", "supplier_invoice_line", "stock_movement", "price_observation", "sku_alias"],
                "actions": []},
            "reviewer": {"read": ["*"], "write": [],
                "actions": ["merge_changeset", "settle_price_observation"]}
        }
    });
    let shell = ce_tools::compose(&shell, &["period", "audit", "changesets", "invoicing"]).unwrap();
    let mut m = ce_plugin::compose(&shell, "business.inventory", None)
        .unwrap()
        .manifest;
    // Received stock is part of the invoice: it goes through the same review.
    m["entities"]["stock_movement"]["governed"] = json!(true);
    m
}

impl Api {
    async fn act(&mut self, name: &str, id: &str, version: u64) -> (StatusCode, Value) {
        self.call(
            "POST",
            &format!("/v1/{INST}/actions/{name}"),
            Some(version),
            Some(json!({"id": id, "input": {}})),
        )
        .await
    }
}

fn id(v: &Value) -> String {
    v["data"]["id"].as_str().unwrap().to_string()
}

const INVOICE: &str = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

/// The clerk stages one invoice for "Sysco" (number given) with a known and an unknown product.
async fn stage(clerk: &mut Api, number: &str, line: &str, item: &str, period: &str) -> String {
    let (s, v) = clerk
        .post(
            "supplier_invoice",
            json!({"id": INVOICE, "vendor": "Sysco", "invoice_no": number,
                "invoice_date": "2026-11-03", "fuel_charge": "2.00", "stated_total": "272.50"}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let cs = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let rows = [
        (
            "supplier_invoice_line",
            json!({"invoice": INVOICE, "line_no": 1, "vendor_sku": "S-1",
            "item": item, "qty": "5", "unit_price": "50"}),
        ),
        (
            "supplier_invoice_line",
            json!({"invoice": INVOICE, "line_no": 2, "vendor_sku": "S-NEW",
            "description": "unknown product", "qty": "2", "unit_price": "10.25"}),
        ),
        (
            "stock_movement",
            json!({"line": line, "period": period, "received": "5",
            "txn_date": "2026-11-03"}),
        ),
        (
            "price_observation",
            json!({"line": line, "observed_price": "50", "source": "Sysco-123"}),
        ),
    ];
    for (entity, body) in rows {
        let (s, v) = clerk
            .call_with(
                "POST",
                &format!("/v1/{INST}/{entity}"),
                None,
                Some(body),
                &cs,
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{entity}: {v}");
    }
    cs
}

#[tokio::test]
async fn an_invoice_is_staged_merged_once_and_settles_its_prices() {
    let Some(url) = url() else { return };
    let app = app(&url, manifest()).await;
    let mut owner = api("owner", app.clone()).await;
    let mut clerk = as_role(&owner, "clerk");
    let mut rev = as_role(&owner, "reviewer");

    // Setup, written directly by the owner: a period, an item at 40, a count of 10.
    let (_, period) = owner
        .post(
            "period",
            json!({"label": "2026-W1", "starts_on": "2026-11-01", "ends_on": "2026-11-07"}),
        )
        .await;
    let (_, item) = owner
        .post(
            "stock_item",
            json!({"sku": "A", "description": "flour", "unit_price": "40"}),
        )
        .await;
    let (_, line) = owner
        .post(
            "stock_line",
            json!({"item": id(&item), "period": id(&period),
            "opening_oh": "10", "unit_price": "40"}),
        )
        .await;
    let (line, item, period) = (id(&line), id(&item), id(&period));

    // Staged, nothing applied.
    let cs = stage(&mut clerk, "123", &line, &item, &period).await;
    assert!(rev.list("supplier_invoice").await.is_empty());
    assert!(rev.list("stock_movement").await.is_empty());
    // The clerk cannot approve their own work.
    let (s, _) = clerk.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // A reviewer can see what the merge would store, reconciliation included, before deciding.
    let (s, _) = clerk.verb(&cs, "preview").await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = rev.verb(&cs, "preview").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["mergeable"], true, "{v}");
    assert!(v["data"]["failure"].is_null());
    let results = v["data"]["results"].as_array().unwrap();
    let inv_op = results
        .iter()
        .find(|r| r["entity"] == "supplier_invoice")
        .expect("the invoice is in the preview");
    assert_eq!(inv_op["record"]["data"]["goods_total"], "270.50");
    assert_eq!(inv_op["record"]["data"]["delta"], "0.00");
    assert_eq!(inv_op["record"]["data"]["reconciled"], true);
    // ...and previewing kept nothing: no records, and the changeset is still open.
    assert!(rev.list("supplier_invoice").await.is_empty());
    assert!(rev.list("stock_movement").await.is_empty());

    // The manager merges: the invoice, its lines, the stock and the observation land together.
    let (s, v) = rev.verb(&cs, "merge").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let invoices = rev.list("supplier_invoice").await;
    assert_eq!(invoices.len(), 1);
    let inv = &invoices[0];
    assert_eq!(inv["id"], INVOICE);
    assert_eq!(inv["goods_total"], "270.50");
    assert_eq!(inv["net_total"], "272.50");
    assert_eq!(inv["delta"], "0.00");
    assert_eq!(inv["reconciled"], true);
    assert_eq!(rev.list("supplier_invoice_line").await.len(), 2);
    // The unknown product is queued: its line has no item yet.
    let unknown: Vec<_> = rev
        .list("supplier_invoice_line")
        .await
        .into_iter()
        .filter(|l| l["item"].is_null())
        .collect();
    assert_eq!(unknown.len(), 1);
    assert_eq!(unknown[0]["vendor_sku"], "S-NEW");
    let got = owner.get_line(&line).await;
    assert_eq!(got["received"], "5.000");
    assert_eq!(got["ending_qty"], "15.000");
    assert_eq!(got["unit_price"], "40.000000", "price waits for the settle");

    // Settling the observation: exactly +25% applies, and the line takes the new price.
    let obs = rev.list("price_observation").await;
    assert_eq!(obs.len(), 1);
    let (s, v) = rev
        .act(
            "settle_price_observation",
            obs[0]["id"].as_str().unwrap(),
            1,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["status"], "auto_applied");
    assert_eq!(owner.get_line(&line).await["unit_price"], "50.000000");

    // The same invoice again (any case or spacing) is refused whole: nothing is doubled.
    let (s, v) = clerk
        .post(
            "supplier_invoice",
            json!({"id": "7c9e6679-7425-40de-944b-e07fc1f90ae8", "vendor": " SYSCO ",
                "invoice_no": "123", "stated_total": "1"}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let dup = v["data"]["changeset_id"].as_str().unwrap().to_string();
    let (s, v) = clerk
        .call_with(
            "POST",
            &format!("/v1/{INST}/stock_movement"),
            None,
            Some(json!({"line": line, "period": period, "received": "5"})),
            &dup,
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    // The preview says so before anyone merges: it names the operation that fails.
    let (s, v) = rev.verb(&dup, "preview").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["mergeable"], false, "{v}");
    assert_eq!(v["data"]["failure"]["entity"], "supplier_invoice", "{v}");
    let (s, v) = rev.verb(&dup, "merge").await;
    assert!(!s.is_success(), "{s} {v}");
    assert_eq!(rev.list("supplier_invoice").await.len(), 1);
    assert_eq!(
        rev.list("stock_movement").await.len(),
        1,
        "no second receipt"
    );
    assert_eq!(owner.get_line(&line).await["received"], "5.000");

    // An invoice whose parts do not add up to its printed total is stored, and flagged.
    let (s, v) = owner
        .post(
            "supplier_invoice",
            json!({"vendor": "Sysco", "invoice_no": "124", "stated_total": "100"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["data"]["delta"], "100.00");
    assert_eq!(v["data"]["reconciled"], false);
    close(vec![app, owner.app, clerk.app, rev.app]).await;
}

impl Api {
    async fn get_line(&mut self, line: &str) -> Value {
        let (s, v) = self
            .call("GET", &format!("/v1/{INST}/stock_line/{line}"), None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"].clone()
    }
}
