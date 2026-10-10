use crate::{Record, Store, StoreError, TenantScope};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

type RecordKey = (String, String, String); // tenant, entity, id

struct Stored {
    record: Record,
    deleted: bool,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    records: HashMap<RecordKey, Stored>,
    /// (tenant, key) -> (payload, snapshot of the original insert result)
    keys: HashMap<(String, String), (Value, Record)>,
}

/// Reference backend for tests and local development. Not for production.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn key(scope: &TenantScope, entity: &str, id: &str) -> RecordKey {
    (scope.tenant_id().into(), entity.into(), id.into())
}

impl Store for MemoryStore {
    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        let mut g = self.lock();
        let idem = idempotency_key.map(|k| (scope.tenant_id().to_string(), k.to_string()));
        if let Some(k) = &idem {
            if let Some((payload, original)) = g.keys.get(k) {
                if original.entity != entity || payload != &data {
                    return Err(StoreError::IdempotencyKeyReused);
                }
                return Ok(original.clone());
            }
        }
        g.next_id += 1;
        let id = format!("rec-{}", g.next_id);
        let record = Record {
            id: id.clone(),
            entity: entity.into(),
            version: 1,
            data: data.clone(),
        };
        g.records.insert(
            key(scope, entity, &id),
            Stored {
                record: record.clone(),
                deleted: false,
            },
        );
        if let Some(k) = idem {
            g.keys.insert(k, (data, record.clone()));
        }
        Ok(record)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        let g = self.lock();
        match g.records.get(&key(scope, entity, id)) {
            Some(s) if !s.deleted => Ok(s.record.clone()),
            _ => Err(StoreError::NotFound),
        }
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        let mut g = self.lock();
        match g.records.get_mut(&key(scope, entity, id)) {
            Some(s) if !s.deleted => {
                if s.record.version != expected_version {
                    return Err(StoreError::VersionConflict {
                        current: s.record.version,
                    });
                }
                s.record.version += 1;
                s.record.data = data;
                Ok(s.record.clone())
            }
            _ => Err(StoreError::NotFound),
        }
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let mut g = self.lock();
        match g.records.get_mut(&key(scope, entity, id)) {
            Some(s) if !s.deleted => {
                if s.record.version != expected_version {
                    return Err(StoreError::VersionConflict {
                        current: s.record.version,
                    });
                }
                s.record.version += 1;
                s.deleted = true;
                Ok(())
            }
            _ => Err(StoreError::NotFound),
        }
    }
}
