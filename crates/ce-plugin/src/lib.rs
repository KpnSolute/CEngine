//! CEngine plugin contract, draft v1. See `spec/plugins.md`.
//!
//! A plugin is a `plugin.toml` that fills the policy slots of one registered module with
//! parameters and expressions. It is data: nothing here executes code, opens files or reads
//! the network. Unknown fields, unknown slots, unsupported hosts and over-broad capabilities
//! are rejected, never ignored.

mod load;
mod registry;
mod validate;

pub use load::{
    digest, digest_of, discover, load, Installed, LoadError, Pin, DESCRIPTOR, PLUGINS_DIR,
};
pub use registry::{Module, Registry, Slot};
pub use validate::{parse, Capabilities, Plugin, Policy, Violation, MAX_BYTES, MAX_POLICIES};

/// Version of the `plugin.toml` schema this crate reads.
pub const SCHEMA_VERSION: u32 = 1;
/// The only execution kind in 0.x: rules interpreted by the engine.
pub const KIND_POLICY_PACK: &str = "policy-pack";
