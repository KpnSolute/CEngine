use crate::{Event, NewEvent};
use ce_auth::TenantScope;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Deliveries before an event is dead-lettered.
pub const MAX_ATTEMPTS: u32 = 5;
const BASE_BACKOFF_SECS: u64 = 5;
const MAX_BACKOFF_SECS: u64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    InFlight,
    Delivered,
    DeadLettered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxError {
    NotFound,
    /// The event is not in flight, so it cannot be acked or failed.
    NotInFlight,
    /// The event id is already taken for this tenant.
    Duplicate,
}

/// An event handed to the relay.
#[derive(Debug, Clone, PartialEq)]
pub struct Claimed {
    pub event: Event,
    /// 1 on the first delivery.
    pub attempt: u32,
}

struct Row {
    event: Event,
    status: Status,
    attempts: u32,
    not_before: u64,
    lease_until: u64,
}

#[derive(Default)]
struct Inner {
    next_seq: u64,
    /// seq -> row; BTreeMap keeps delivery order.
    rows: BTreeMap<u64, Row>,
}

/// In-memory reference outbox. The Postgres outbox must pass the same tests.
#[derive(Default)]
pub struct MemoryOutbox {
    inner: Mutex<Inner>,
}

fn backoff(attempts: u32) -> u64 {
    BASE_BACKOFF_SECS
        .saturating_mul(1u64 << attempts.saturating_sub(1).min(20))
        .min(MAX_BACKOFF_SECS)
}

impl MemoryOutbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stamps and stores an event for the scope's tenant. A repeated `id` for the same
    /// tenant is refused, which makes a retried write safe.
    pub fn enqueue(
        &self,
        scope: &TenantScope,
        id: &str,
        new: NewEvent,
        now: u64,
    ) -> Result<(), OutboxError> {
        let mut g = self.lock();
        let tenant = scope.tenant_id();
        if g.rows
            .values()
            .any(|r| r.event.tenant == tenant && r.event.id == id)
        {
            return Err(OutboxError::Duplicate);
        }
        g.next_seq += 1;
        let seq = g.next_seq;
        g.rows.insert(
            seq,
            Row {
                event: Event {
                    id: id.into(),
                    tenant: tenant.into(),
                    time: now,
                    new,
                },
                status: Status::Pending,
                attempts: 0,
                not_before: 0,
                lease_until: 0,
            },
        );
        Ok(())
    }

    /// Hands the relay up to `limit` due events in order, leasing each for `lease_secs`.
    /// An event whose lease ran out is handed out again, so a crashed relay loses nothing.
    /// Within one tenant, a later event is never handed out before an earlier one that is
    /// still pending or leased. A dead-lettered event no longer blocks its tenant, so later
    /// events may pass it; dead letters need a human look.
    pub fn claim(&self, limit: usize, now: u64, lease_secs: u64) -> Vec<Claimed> {
        let mut g = self.lock();
        let mut blocked: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for row in g.rows.values_mut() {
            if out.len() >= limit {
                break;
            }
            let tenant = row.event.tenant.clone();
            match row.status {
                Status::Delivered | Status::DeadLettered => continue,
                Status::InFlight if row.lease_until > now => {
                    blocked.push(tenant);
                    continue;
                }
                _ => {}
            }
            if blocked.contains(&tenant) {
                continue;
            }
            if row.not_before > now {
                blocked.push(tenant);
                continue;
            }
            row.status = Status::InFlight;
            row.attempts += 1;
            row.lease_until = now + lease_secs;
            blocked.push(tenant);
            out.push(Claimed {
                event: row.event.clone(),
                attempt: row.attempts,
            });
        }
        out
    }

    fn row_mut<'a>(g: &'a mut Inner, tenant: &str, id: &str) -> Result<&'a mut Row, OutboxError> {
        let row = g
            .rows
            .values_mut()
            .find(|r| r.event.tenant == tenant && r.event.id == id)
            .ok_or(OutboxError::NotFound)?;
        if row.status != Status::InFlight {
            return Err(OutboxError::NotInFlight);
        }
        Ok(row)
    }

    pub fn ack(&self, tenant: &str, id: &str) -> Result<(), OutboxError> {
        let mut g = self.lock();
        Self::row_mut(&mut g, tenant, id)?.status = Status::Delivered;
        Ok(())
    }

    /// Records a failed delivery. Returns the status the event ended in.
    pub fn fail(&self, tenant: &str, id: &str, now: u64) -> Result<Status, OutboxError> {
        let mut g = self.lock();
        let row = Self::row_mut(&mut g, tenant, id)?;
        if row.attempts >= MAX_ATTEMPTS {
            row.status = Status::DeadLettered;
        } else {
            row.status = Status::Pending;
            row.not_before = now + backoff(row.attempts);
        }
        Ok(row.status)
    }

    pub fn status(&self, tenant: &str, id: &str) -> Option<Status> {
        self.lock()
            .rows
            .values()
            .find(|r| r.event.tenant == tenant && r.event.id == id)
            .map(|r| r.status)
    }
}
