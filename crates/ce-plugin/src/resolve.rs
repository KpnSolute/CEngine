//! Turn a template, a module and an installation's approvals into the manifest to serve.
//! Fails closed: a plugin that was asked for but is not approved, is disabled, was edited after
//! approval, or does not compose is an error, never a silent fall back to the defaults.

use std::path::Path;

use semver::Version;
use serde_json::Value;

use crate::{builtin_registry, compose, load, ComposeError, Composed, Installed, LoadError};

#[derive(Debug)]
pub enum ResolveError {
    /// A plugin was requested but the installation has no active approval for it (never
    /// approved, or disabled).
    NotApproved(String),
    Load(LoadError),
    Compose(ComposeError),
}

/// With `plugin_id`, the plugin must be approved in `installed`; its descriptor is loaded from
/// `root` only if the bytes match the approved digest, then composed with the module. Without
/// one, the module runs on its defaults.
pub fn resolve(
    template: &Value,
    module: &str,
    plugin_id: Option<&str>,
    root: &Path,
    installed: &Installed,
    engine: &Version,
) -> Result<Composed, ResolveError> {
    let Some(id) = plugin_id else {
        return compose(template, module, None).map_err(ResolveError::Compose);
    };
    let pin = installed
        .active(id)
        .ok_or_else(|| ResolveError::NotApproved(id.to_string()))?;
    let plugin = load(root, id, pin, &builtin_registry(), engine).map_err(ResolveError::Load)?;
    compose(template, module, Some((&plugin, &pin.digest))).map_err(ResolveError::Compose)
}
