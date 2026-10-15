//! Plugin deliveries: a record queued in a plugin's entity is sent through a connector the host
//! provides, at least once. The manifest's `deliveries` name the entity, the connector, the
//! operation and the parameters (copied from the record). The plugin holds no credential and
//! makes no call; the host's `Provider` does, and the idempotency key `<entity>:<id>` makes a
//! retry safe.
//!
//! The entity carries `status` (`queued`, `in_flight`, `done`, `dead`), `attempts`, `not_before`
//! and `last_error`; the relay writes them, and `result` when the entity has that field. A failed
//! call backs off and, after `MAX_ATTEMPTS`, the record is `dead` until a person queues it again.
//! One failing job never blocks the others.

use crate::events;
use ce_auth::TenantScope;
use ce_events::{backoff, MAX_ATTEMPTS};
use ce_manifest::Manifest;
use ce_provider::{ConnectorType, Provider, ProviderError, ProviderRequest};
use ce_store::{ListRequest, Store, StoreError};
use serde_json::{json, Map, Value};

/// How long a claimed job is held before another relay may take it again.
const LEASE_SECS: u64 = 60;
const BATCH: usize = 50;
/// The most pages one pass reads per delivery, so a long queue of waiting jobs cannot stall it.
const MAX_PAGES: usize = 20;

pub(crate) fn enabled(m: &Manifest) -> bool {
    events::enabled(m)
        && m.value["deliveries"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub delivered: u32,
    pub retrying: u32,
    pub dead: u32,
}

fn short(msg: &str) -> String {
    msg.chars().take(400).collect()
}

/// One pass for one tenant over every declared delivery.
pub fn run_once(
    store: &dyn Store,
    scope: &TenantScope,
    provider: &dyn Provider,
    m: &Manifest,
    now: u64,
) -> Result<Report, StoreError> {
    let mut report = Report::default();
    for d in m.value["deliveries"].as_array().into_iter().flatten() {
        let (Some(entity), Some(conn), Some(op)) = (
            d["entity"].as_str(),
            d["connector"].as_str().and_then(ConnectorType::parse),
            d["operation"].as_str(),
        ) else {
            continue;
        };
        let keeps_result = m.value["entities"][entity]["fields"]["result"]["type"] == "json";
        let mut req = ListRequest {
            select: None,
            filter: Some(json!({"field": "status", "op": "in", "value": ["queued", "in_flight"]})),
            search: None,
            sort: Vec::new(),
            limit: Some(BATCH),
            after: None,
        };
        // Records are listed in pages, so a run of leased or waiting jobs at the front never hides
        // the ones that are due behind them.
        let mut due = 0;
        for _ in 0..MAX_PAGES {
            let page = store.list(scope, entity, &req)?;
            for rec in page.records {
                let attempts =
                    u32::try_from(rec.data["attempts"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
                if rec.data["not_before"].as_u64().unwrap_or(0) > now {
                    continue; // waiting for a retry, or leased by another relay
                }
                due += 1;
                if rec.data["status"] == "in_flight" && attempts >= MAX_ATTEMPTS {
                    // The relay died on this job every time; it counts as a failed delivery.
                    store.update(
                        scope,
                        entity,
                        &rec.id,
                        rec.version,
                        events::set(&rec, &[("status", json!("dead"))]),
                    )?;
                    report.dead += 1;
                    continue;
                }
                let claimed = match store.update(
                    scope,
                    entity,
                    &rec.id,
                    rec.version,
                    events::set(
                        &rec,
                        &[
                            ("status", json!("in_flight")),
                            ("attempts", json!(attempts + 1)),
                            ("not_before", json!(now + LEASE_SECS)),
                        ],
                    ),
                ) {
                    Ok(c) => c,
                    Err(StoreError::VersionConflict { .. }) => continue, // another relay has it
                    Err(e) => return Err(e),
                };
                let mut params = Map::new();
                for (k, v) in d["params"].as_object().into_iter().flatten() {
                    let val = match v["from"].as_str() {
                        Some("id") => json!(rec.id),
                        Some(f) => rec.data[f].clone(),
                        None => v["value"].clone(),
                    };
                    params.insert(k.clone(), val);
                }
                let call = ProviderRequest::write(
                    conn,
                    op,
                    Value::Object(params),
                    &format!("{entity}:{}", rec.id),
                );
                let mut edits: Vec<(&str, Value)> = Vec::new();
                match provider.call(scope, &call) {
                    Ok(answer) => {
                        report.delivered += 1;
                        edits.push(("status", json!("done")));
                        edits.push(("last_error", Value::Null));
                        if keeps_result {
                            edits.push(("result", answer.data));
                        }
                    }
                    Err(e) => {
                        let (why, retryable) = match e {
                            ProviderError::Unavailable => {
                                ("the connector is unavailable".into(), true)
                            }
                            ProviderError::Failed(m) => (m, true),
                            ProviderError::Invalid(m) => (m.to_string(), false),
                            ProviderError::ApprovalRequired => {
                                ("approval required".to_string(), false)
                            }
                        };
                        if retryable && attempts + 1 < MAX_ATTEMPTS {
                            report.retrying += 1;
                            edits.push(("status", json!("queued")));
                            edits.push(("not_before", json!(now + backoff(attempts + 1))));
                        } else {
                            report.dead += 1;
                            edits.push(("status", json!("dead")));
                        }
                        edits.push(("last_error", json!(short(&why))));
                    }
                }
                store.update(
                    scope,
                    entity,
                    &claimed.id,
                    claimed.version,
                    events::set(&claimed, &edits),
                )?;
            }
            if due >= BATCH {
                break;
            }
            match page.next {
                Some(cursor) => req.after = Some(cursor),
                None => break,
            }
        }
    }

    Ok(report)
}
