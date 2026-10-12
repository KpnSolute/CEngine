//! The module registry: which modules exist, which slots they expose, which capabilities a
//! plugin may ask for. Controlled by CEngine, never by a plugin.

use std::collections::BTreeMap;

/// One policy slot a module lets a plugin fill.
#[derive(Debug, Clone)]
pub struct Slot {
    pub name: String,
    /// `Some(true)`: a rule is required; `Some(false)`: a rule is forbidden; `None`: optional.
    pub rule: Option<bool>,
    /// Parameter names the slot accepts.
    pub params: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Module {
    pub id: String,
    /// Major version of the module's plugin API.
    pub api: u32,
    pub slots: Vec<Slot>,
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub emit: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    modules: BTreeMap<String, Module>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, module: Module) -> Self {
        self.modules.insert(module.id.clone(), module);
        self
    }

    pub fn module(&self, id: &str) -> Option<&Module> {
        self.modules.get(id)
    }
}
