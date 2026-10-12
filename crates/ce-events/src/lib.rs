//! CEngine events (draft v1): the envelope, the signature and the outbox.
//!
//! - **Envelope:** CloudEvents 1.0 JSON. Type is `com.kpnsolute.compute.{name}.v{n}`, where
//!   `name` is the manifest's dotted emit name (`request.approved` or `inventory.period.closed`).
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

pub use outbox::{backoff, Claimed, MemoryOutbox, OutboxError, Status, MAX_ATTEMPTS};
pub use sign::{SignError, Signer, KEY_OVERLAP_SECS};

use serde_json::{json, Value};

/// What a template or the engine wants to publish, before it is stamped.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// Dotted name with 2 to 4 segments, e.g. `request.approved` or `inventory.period.closed`.
    pub name: String,
    /// Contract version of this event type, starting at 1.
    pub version: u32,
    /// The record the event is about.
    pub subject: Option<String>,
    pub data: Value,
}

/// `com.kpnsolute.compute.{name}.v{n}`
pub fn event_type(name: &str, version: u32) -> String {
    format!("com.kpnsolute.compute.{name}.v{version}")
}

/// Checks a manifest `emit` name: 2 to 4 dot-separated segments, each `[a-z][a-z0-9_]*`.
pub fn valid_event_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            let b = p.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_lowercase()
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        })
}

/// Unix seconds as an RFC 3339 UTC timestamp, e.g. `2026-10-10T19:30:00Z`.
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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
            "type": event_type(&self.new.name, self.new.version),
            "time": rfc3339(self.time),
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
