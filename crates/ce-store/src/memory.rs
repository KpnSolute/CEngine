use crate::{Record, Store, StoreError, TenantScope};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;

type RecordKey = (String, String, String); // tenant, entity, id

#[derive(Clone)]
struct Stored {
    record: Record,
    deleted: bool,
}

#[derive(Default, Clone)]
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

impl Inner {
    fn insert(
        &mut self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        caller: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        let idem = idempotency_key.map(|k| (scope.tenant_id().to_string(), k.to_string()));
        if let Some(k) = &idem {
            if let Some((payload, original)) = self.keys.get(k) {
                if original.entity != entity || payload != caller.unwrap_or(&data) {
                    return Err(StoreError::IdempotencyKeyReused);
                }
                return Ok(original.clone());
            }
        }
        // A caller-chosen id (validated by the host) is used as given; it must be unused.
        let payload = caller.cloned().unwrap_or_else(|| data.clone());
        let mut data = data;
        let id = match data.as_object_mut().and_then(|o| o.remove("id")) {
            Some(Value::String(chosen)) => {
                if self.records.contains_key(&key(scope, entity, &chosen)) {
                    return Err(StoreError::Constraint("unique"));
                }
                chosen
            }
            _ => {
                self.next_id += 1;
                format!("rec-{}", self.next_id)
            }
        };
        let record = Record {
            id: id.clone(),
            entity: entity.into(),
            version: 1,
            data: data.clone(),
        };
        self.records.insert(
            key(scope, entity, &id),
            Stored {
                record: record.clone(),
                deleted: false,
            },
        );
        if let Some(k) = idem {
            self.keys.insert(k, (payload, record.clone()));
        }
        Ok(record)
    }

    fn replay(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: &Value,
        key: &str,
    ) -> Result<Option<Record>, StoreError> {
        match self
            .keys
            .get(&(scope.tenant_id().to_string(), key.to_string()))
        {
            None => Ok(None),
            Some((payload, original)) if original.entity == entity && payload == caller => {
                Ok(Some(original.clone()))
            }
            Some(_) => Err(StoreError::IdempotencyKeyReused),
        }
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        match self.records.get(&key(scope, entity, id)) {
            Some(s) if !s.deleted => Ok(s.record.clone()),
            _ => Err(StoreError::NotFound),
        }
    }

    fn update(
        &mut self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        match self.records.get_mut(&key(scope, entity, id)) {
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
        &mut self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        match self.records.get_mut(&key(scope, entity, id)) {
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

    fn child_values(
        &self,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Vec<Value> {
        self.records
            .iter()
            .filter(|((tenant, ent, _), s)| {
                tenant == scope.tenant_id()
                    && ent == entity
                    && !s.deleted
                    && s.record.data[ref_field].as_str() == Some(parent_id)
            })
            .map(|(_, s)| match attr {
                Some(a) => s.record.data.get(a).cloned().unwrap_or(Value::Null),
                None => Value::Bool(true),
            })
            .collect()
    }
}

impl Store for MemoryStore {
    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        self.lock()
            .insert(scope, entity, data, None, idempotency_key)
    }

    fn insert_with(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: Value,
        derive: &mut dyn FnMut() -> Result<Map<String, Value>, StoreError>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        // The first result is looked up before `derive` runs, and no lock is held while it does
        // (it reads through this same store). Two direct callers with one new key can therefore
        // both derive; the server never does that, because it calls this inside `atomic`, which
        // holds the store's lock for the whole write.
        if let Some(key) = idempotency_key {
            if let Some(first) = self.lock().replay(scope, entity, &caller, key)? {
                return Ok(first);
            }
        }
        let mut body = caller.clone();
        if let Value::Object(o) = &mut body {
            o.extend(derive()?);
        }
        self.lock()
            .insert(scope, entity, body, Some(&caller), idempotency_key)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        self.lock().get(scope, entity, id)
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        self.lock()
            .update(scope, entity, id, expected_version, data)
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.lock().delete(scope, entity, id, expected_version)
    }

    fn child_values(
        &self,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .lock()
            .child_values(scope, entity, ref_field, parent_id, attr))
    }

    /// Holds the store's lock for the whole closure and works on a copy, which replaces the
    /// real state only if the closure succeeds.
    fn atomic(
        &self,
        f: &mut dyn FnMut(&dyn Store) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut g = self.lock();
        let tx = MemoryTx {
            inner: RefCell::new(g.clone()),
        };
        f(&tx)?;
        *g = tx.inner.into_inner();
        Ok(())
    }
}

/// The store a closure sees inside `MemoryStore::atomic`.
struct MemoryTx {
    inner: RefCell<Inner>,
}

impl Store for MemoryTx {
    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        self.inner
            .borrow_mut()
            .insert(scope, entity, data, None, idempotency_key)
    }

    fn insert_with(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: Value,
        derive: &mut dyn FnMut() -> Result<Map<String, Value>, StoreError>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        // The first result is looked up before `derive` runs, and no lock is held while it does
        // (it reads through this same store). Two direct callers with one new key can therefore
        // both derive; the server never does that, because it calls this inside `atomic`, which
        // holds the store's lock for the whole write.
        if let Some(key) = idempotency_key {
            if let Some(first) = self.inner.borrow().replay(scope, entity, &caller, key)? {
                return Ok(first);
            }
        }
        let mut body = caller.clone();
        if let Value::Object(o) = &mut body {
            o.extend(derive()?);
        }
        self.inner
            .borrow_mut()
            .insert(scope, entity, body, Some(&caller), idempotency_key)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        self.inner.borrow().get(scope, entity, id)
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        self.inner
            .borrow_mut()
            .update(scope, entity, id, expected_version, data)
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.inner
            .borrow_mut()
            .delete(scope, entity, id, expected_version)
    }

    fn child_values(
        &self,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .inner
            .borrow()
            .child_values(scope, entity, ref_field, parent_id, attr))
    }

    fn atomic(
        &self,
        f: &mut dyn FnMut(&dyn Store) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        f(self)
    }
}
