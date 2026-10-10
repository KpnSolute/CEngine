//! Runs the shared contract against the in-memory backend.
use ce_store::{contract, MemoryStore};

#[test]
fn memory_store_passes_the_contract() {
    contract::run_all(&MemoryStore::new);
}
