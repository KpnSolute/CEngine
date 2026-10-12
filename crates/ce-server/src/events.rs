//! Events out: what a template emits is stored in `outbox_event` in the transaction of the
//! change that caused it, then a relay delivers it signed (CloudEvents, HMAC-SHA256), at least
//! once. A failed delivery backs off and, after `MAX_ATTEMPTS`, the event is dead-lettered; a
//! person replays it. Delivery itself is the host's (`Deliver`): CEngine does not call providers.
//!
//! Events of one tenant leave in the order they were stored (`seq`): a later event never goes
//! out before an earlier one that is waiting for a retry. A dead-lettered event no longer blocks.

use crate::writes::Env;
use ce_auth::TenantScope;
use ce_events::{backoff, Event, NewEvent, Signer, MAX_ATTEMPTS};
use ce_manifest::Manifest;
use ce_store::{ListRequest, SortDir, SortKey, Store, StoreError};
use serde_json::{json, Map, Value};

pub(crate) const ENTITY: &str = "outbox_event";
/// The role action that lets a caller replay events and run the relay.
pub(crate) const ACTION: &str = "manage_events";
/// How long a claimed event is held before another relay may take it again.
const LEASE_SECS: u64 = 60;
const BATCH: usize = 50;

pub(crate) fn enabled(m: &Manifest) -> bool {
    m.value["entities"][ENTITY].is_object() && m.value["actions"][ACTION].is_object()
}

fn seq() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Stores an event in the caller's transaction when the template uses the `events` tool. A name
/// the envelope cannot carry is dropped, never an error: the data change stands.
pub(crate) fn enqueue(
    tx: &dyn Store,
    env: &Env,
    name: &str,
    subject: Option<&str>,
    data: Value,
) -> Result<(), StoreError> {
    if !enabled(env.manifest) || !ce_events::valid_event_name(name) {
        return Ok(());
    }
    let mut ev = Map::new();
    ev.insert("name".into(), json!(name));
    ev.insert("data".into(), data);
    ev.insert("at".into(), json!(env.ctx.now_unix));
    ev.insert("seq".into(), json!(seq()));
    if let Some(s) = subject {
        ev.insert("subject".into(), json!(s));
    }
    tx.insert(env.scope, ENTITY, Value::Object(ev), None)?;
    Ok(())
}

/// What the host's transport gets for one event.
pub struct Delivery<'a> {
    pub event_id: &'a str,
    pub event_type: String,
    /// The exact bytes to send; the signature covers them.
    pub payload: &'a [u8],
    pub key_id: &'a str,
    pub signature: &'a str,
}

/// Sends one event. `Err` carries a short reason for `last_error`.
pub trait Deliver: Send + Sync {
    fn deliver(&self, d: &Delivery) -> Result<(), String>;
}

pub struct Relay {
    pub signer: Signer,
    pub deliverer: Box<dyn Deliver>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub delivered: u32,
    pub retrying: u32,
    pub dead_lettered: u32,
}

fn set(rec: &ce_store::Record, edits: &[(&str, Value)]) -> Value {
    let mut d = rec.data.clone();
    if let Some(o) = d.as_object_mut() {
        for (k, v) in edits {
            // An update replaces the whole body, so "no value" is a missing key.
            if v.is_null() {
                o.remove(*k);
            } else {
                o.insert((*k).into(), v.clone());
            }
        }
    }
    d
}

fn short(msg: &str) -> String {
    msg.chars().take(400).collect()
}

/// One pass for one tenant: claims due events in order and delivers them. A host calls this on
/// a timer for each tenant; two relays at once are safe because a claim needs the record's version.
pub fn relay_once(
    store: &dyn Store,
    scope: &TenantScope,
    relay: &Relay,
    now: u64,
) -> Result<Report, StoreError> {
    let req = ListRequest {
        select: None,
        filter: Some(json!({"field": "status", "op": "in", "value": ["pending", "in_flight"]})),
        search: None,
        sort: vec![SortKey {
            field: "seq".into(),
            dir: SortDir::Asc,
        }],
        limit: Some(BATCH),
        after: None,
    };
    let mut report = Report::default();
    for rec in store.list(scope, ENTITY, &req)?.records {
        let d = &rec.data;
        let attempts = u32::try_from(d["attempts"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
        let not_before = d["not_before"].as_u64().unwrap_or(0);
        let in_flight = d["status"] == "in_flight";
        if not_before > now {
            break; // waiting for a retry or leased by another relay: later events wait too
        }
        if in_flight && attempts >= MAX_ATTEMPTS {
            // The relay died on this event every time; it counts as a failed delivery.
            store.update(
                scope,
                ENTITY,
                &rec.id,
                rec.version,
                set(&rec, &[("status", json!("dead_lettered"))]),
            )?;
            report.dead_lettered += 1;
            continue;
        }
        let claimed = match store.update(
            scope,
            ENTITY,
            &rec.id,
            rec.version,
            set(
                &rec,
                &[
                    ("status", json!("in_flight")),
                    ("attempts", json!(attempts + 1)),
                    ("not_before", json!(now + LEASE_SECS)),
                ],
            ),
        ) {
            Ok(c) => c,
            Err(StoreError::VersionConflict { .. }) => break, // another relay has it
            Err(e) => return Err(e),
        };
        let event = Event {
            id: rec.id.clone(),
            tenant: scope.tenant_id().to_string(),
            time: d["at"].as_u64().unwrap_or(now),
            new: NewEvent {
                name: d["name"].as_str().unwrap_or_default().to_string(),
                version: u32::try_from(d["event_version"].as_u64().unwrap_or(1)).unwrap_or(1),
                subject: d["subject"].as_str().map(String::from),
                data: d["data"].clone(),
            },
        };
        let payload = event.payload();
        let (key_id, signature) = relay.signer.sign(&payload);
        let sent = relay.deliverer.deliver(&Delivery {
            event_id: &event.id,
            event_type: ce_events::event_type(&event.new.name, event.new.version),
            payload: &payload,
            key_id: &key_id,
            signature: &signature,
        });
        let mut retry = false;
        let edits: Vec<(&str, Value)> = match sent {
            Ok(()) => {
                report.delivered += 1;
                vec![
                    ("status", json!("delivered")),
                    ("delivered_at", json!(now)),
                    ("last_error", Value::Null),
                ]
            }
            Err(why) if attempts + 1 >= MAX_ATTEMPTS => {
                report.dead_lettered += 1;
                vec![
                    ("status", json!("dead_lettered")),
                    ("last_error", json!(short(&why))),
                ]
            }
            Err(why) => {
                report.retrying += 1;
                retry = true;
                vec![
                    ("status", json!("pending")),
                    ("not_before", json!(now + backoff(attempts + 1))),
                    ("last_error", json!(short(&why))),
                ]
            }
        };
        store.update(
            scope,
            ENTITY,
            &claimed.id,
            claimed.version,
            set(&claimed, &edits),
        )?;
        if retry {
            break; // later events wait for this one
        }
    }
    Ok(report)
}

/// Puts a delivered or dead-lettered event back in the queue.
pub(crate) fn replay(
    store: &dyn Store,
    scope: &TenantScope,
    id: &str,
) -> Result<ce_store::Record, StoreError> {
    let rec = store.get(scope, ENTITY, id)?;
    if !matches!(
        rec.data["status"].as_str(),
        Some("delivered" | "dead_lettered")
    ) {
        return Err(StoreError::Constraint("not_replayable"));
    }
    store.update(
        scope,
        ENTITY,
        id,
        rec.version,
        set(
            &rec,
            &[
                ("status", json!("pending")),
                ("attempts", json!(0)),
                ("not_before", json!(0)),
                ("last_error", Value::Null),
                ("delivered_at", Value::Null),
            ],
        ),
    )
}
