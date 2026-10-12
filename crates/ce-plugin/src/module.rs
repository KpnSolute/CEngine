//! Built-in modules: a module is a manifest fragment with named policy slots. A plugin fills
//! slots with rules; unfilled slots take the module's default. Composition checks the result
//! with `ce-manifest`, so a rule that does not type-check is refused when the plugin is
//! installed, never at request time.

use serde_json::{Map, Value};

use crate::{Module, Plugin, Registry, Slot};

const MODULES: &[&str] = &[include_str!("../modules/inventory.json")];

fn defs() -> Vec<Value> {
    MODULES
        .iter()
        .map(|s| serde_json::from_str(s).expect("built-in modules are valid JSON"))
        .collect()
}

/// Registry of the built-in modules, for validating plugins against.
pub fn builtin_registry() -> Registry {
    let mut reg = Registry::new();
    for d in defs() {
        let strings = |k: &str| -> Vec<String> {
            d[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        let slots = d["slots"]
            .as_object()
            .map(|o| {
                o.keys()
                    .map(|k| Slot {
                        name: k.clone(),
                        rule: None,
                        params: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        reg = reg.with(Module {
            id: d["module"].as_str().expect("module id").to_string(),
            api: d["api"].as_u64().expect("module api") as u32,
            slots,
            read: strings("read"),
            write: strings("write"),
            emit: strings("emit"),
        });
    }
    reg
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComposeError {
    UnknownModule(String),
    /// The plugin targets a different module or API major than the one being composed.
    PluginMismatch,
    NotAnObject,
    /// The template lacks an entity the module needs (for example `period`).
    MissingEntity(String),
    /// The template already has an entity the module adds.
    Clash(String),
    /// The composed manifest failed validation: `(code, path, message)`.
    Invalid(Vec<(String, String, String)>),
}

/// A composed manifest and its canonical hash.
#[derive(Debug, Clone)]
pub struct Composed {
    pub manifest: Value,
    pub hash: String,
}

/// Add `module` to `template`, with `plugin`'s rules in its slots (defaults where the plugin is
/// silent, or when there is no plugin). The template's roles are untouched. The result is
/// validated before it is returned.
pub fn compose(
    template: &Value,
    module: &str,
    plugin: Option<(&Plugin, &str)>,
) -> Result<Composed, ComposeError> {
    let def = defs()
        .into_iter()
        .find(|d| d["module"] == module)
        .ok_or_else(|| ComposeError::UnknownModule(module.to_string()))?;
    let (plugin, digest) = (plugin.map(|(p, _)| p), plugin.map(|(_, d)| d));
    if let Some(p) = plugin {
        if p.module != module || Some(u64::from(p.module_api)) != def["api"].as_u64() {
            return Err(ComposeError::PluginMismatch);
        }
    }
    let mut out = template.clone();
    let root = out.as_object_mut().ok_or(ComposeError::NotAnObject)?;
    let entities = root
        .entry("entities")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(ComposeError::NotAnObject)?;
    for need in def["requires_entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !entities.contains_key(need) {
            return Err(ComposeError::MissingEntity(need.to_string()));
        }
    }

    let mut rules: Map<String, Value> = Map::new();
    for (name, slot) in def["slots"].as_object().into_iter().flatten() {
        let chosen = plugin
            .and_then(|p| p.policies.iter().find(|x| &x.slot == name))
            .and_then(|x| x.rule.clone())
            .map(Value::String)
            .unwrap_or_else(|| slot["default"].clone());
        rules.insert(name.clone(), chosen);
    }
    let mut fragment = def["fragment"].clone();
    fill(&mut fragment, &rules);
    for (k, v) in fragment["entities"].as_object().into_iter().flatten() {
        if entities.contains_key(k) {
            return Err(ComposeError::Clash(k.clone()));
        }
        entities.insert(k.clone(), v.clone());
    }

    let mut prov = serde_json::json!({"module": {"name": module, "api": def["api"]}});
    if let (Some(p), Some(d)) = (plugin, digest) {
        prov["plugin"] =
            serde_json::json!({"id": p.id, "version": p.version.to_string(), "digest": d});
    }
    out.as_object_mut()
        .expect("checked above")
        .insert("provenance".into(), prov);

    match ce_manifest::validate(out.to_string().as_bytes()) {
        Ok(m) => Ok(Composed {
            manifest: m.value,
            hash: m.hash,
        }),
        Err(issues) => Err(ComposeError::Invalid(
            issues
                .into_iter()
                .map(|i| (i.code.to_string(), i.path, i.message))
                .collect(),
        )),
    }
}

/// Replace every `{"$slot": name}` with the chosen rule text.
fn fill(v: &mut Value, rules: &Map<String, Value>) {
    match v {
        Value::Object(o) => {
            if o.len() == 1 {
                if let Some(Value::String(name)) = o.get("$slot") {
                    if let Some(rule) = rules.get(name) {
                        *v = rule.clone();
                        return;
                    }
                }
            }
            for x in o.values_mut() {
                fill(x, rules);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| fill(x, rules)),
        _ => {}
    }
}
