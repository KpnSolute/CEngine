//! CEngine events (draft v1): the envelope, the signature and the outbox.
//!
//! - **Envelope:** CloudEvents 1.0 JSON. Type is `com.kpnsolute.compute.{entity}.{verb}.v{n}`.
//!   The tenant comes from the `TenantScope` at enqueue time, never from the caller.
//! - **Signing:** HMAC-SHA256 over the exact payload bytes. A rotated key stays valid for
//!   10 minutes, so receivers need not update in lockstep.
//! - **Outbox:** the event is written in the same transaction as the data change (the caller
//!   does that with the Postgres store later; here it is in memory). A relay claims events in
//!   order, delivers them, then acks or fails them. A failure backs off exponentially and
//!   dead-letters after `MAX_ATTEMPTS`. A delivery failure never undoes the write.
//!
//! Time is passed in as unix seconds so every rule is deterministic and testable.

mod outbox;
mod sign;

pub use outbox::{Claimed, MemoryOutbox, OutboxError, Status, MAX_ATTEMPTS};
pub use sign::{SignError, Signer, KEY_OVERLAP_SECS};

use serde_json::{json, Value};

/// What a template or the engine wants to publish, before it is stamped.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// Entity and verb, e.g. `request` and `approved`.
    pub entity: String,
    pub verb: String,
    /// Contract version of this event type, starting at 1.
    pub version: u32,
    /// The record the event is about.
    pub subject: Option<String>,
    pub data: Value,
}

/// `com.kpnsolute.compute.{entity}.{verb}.v{n}`
pub fn event_type(entity: &str, verb: &str, version: u32) -> String {
    format!("com.kpnsolute.compute.{entity}.{verb}.v{version}")
}

/// Splits a manifest `emit` name such as `request.approved` into entity and verb.
pub fn parse_emit(name: &str) -> Option<(String, String)> {
    let (entity, verb) = name.split_once('.')?;
    if entity.is_empty() || verb.is_empty() || verb.contains('.') {
        return None;
    }
    Some((entity.to_string(), verb.to_string()))
}

/// A stamped event, ready to store and send.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub id: String,
    pub tenant: String,
    pub time: u64,
    pub new: NewEvent,
}

impl Event {
    /// CloudEvents 1.0 structured-mode JSON. `tenant` rides as the `tenant` extension attribute.
    pub fn to_cloud_event(&self) -> Value {
        let mut v = json!({
            "specversion": "1.0",
            "id": self.id,
            "source": "/kpnsolute/compute",
            "type": event_type(&self.new.entity, &self.new.verb, self.new.version),
            "time": self.time,
            "datacontenttype": "application/json",
            "tenant": self.tenant,
            "data": self.new.data,
        });
        if let Some(s) = &self.new.subject {
            v["subject"] = json!(s);
        }
        v
    }

    /// The exact bytes that get signed and sent.
    pub fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_cloud_event()).expect("event serialises")
    }
}
