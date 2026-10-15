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
    /// The template's entity lacks a field the module reads: `(entity, field)`.
    MissingField(String, String),
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
    for (entity, fields) in def["requires_fields"].as_object().into_iter().flatten() {
        for f in fields
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if entities
                .get(entity)
                .and_then(|e| e["fields"].get(f))
                .is_none()
            {
                return Err(ComposeError::MissingField(entity.clone(), f.to_string()));
            }
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

    if let Some(p) = plugin {
        add_plugin_entities(entities, p)?;
        if !p.subscriptions.is_empty() && !entities.contains_key("outbox_event") {
            // Reactions run when an event is stored, which needs the template's `events` tool.
            return Err(ComposeError::MissingEntity("outbox_event".into()));
        }
    }

    if let Some(acts) = fragment["actions"].as_object() {
        let actions = root
            .entry("actions")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .ok_or(ComposeError::NotAnObject)?;
        for (k, v) in acts {
            if actions.contains_key(k) {
                return Err(ComposeError::Clash(k.clone()));
            }
            actions.insert(k.clone(), v.clone());
        }
    }

    if let Some(p) = plugin {
        if !p.subscriptions.is_empty() {
            let prefix = crate::entity_prefix(&p.id);
            let subs = root
                .entry("subscriptions")
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or(ComposeError::NotAnObject)?;
            for s in &p.subscriptions {
                subs.push(serde_json::json!({
                    "event": s.event,
                    "create": {"entity": format!("{prefix}__{}", s.entity), "values": s.values},
                }));
            }
        }
    }

    let mut prov = serde_json::json!({"module": {"name": module, "api": def["api"]}});
    if let (Some(p), Some(d)) = (plugin, digest) {
        prov["plugin"] =
            serde_json::json!({"id": p.id, "version": p.version.to_string(), "digest": d});
    }
    out.as_object_mut()
        .expect("checked above")
        .insert("provenance".into(), prov);

    match ce_manifest::validate_composed(out.to_string().as_bytes()) {
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

/// Install the plugin's own entities as `<prefix>__<name>`. A `ref` to another of the plugin's
/// entities is rewritten to the namespaced name; any other `ref` must name an entity the
/// composed manifest already has, which `ce-manifest` checks on the result.
fn add_plugin_entities(
    entities: &mut Map<String, Value>,
    plugin: &Plugin,
) -> Result<(), ComposeError> {
    let prefix = crate::entity_prefix(&plugin.id);
    let local: std::collections::BTreeSet<&str> =
        plugin.entities.iter().map(|e| e.name.as_str()).collect();
    for e in &plugin.entities {
        let full = format!("{prefix}__{}", e.name);
        if entities.contains_key(&full) {
            return Err(ComposeError::Clash(full));
        }
        let mut def = e.definition.clone();
        if let Some(fields) = def.get_mut("fields").and_then(Value::as_object_mut) {
            for f in fields.values_mut() {
                let target = f.get("ref").and_then(Value::as_str).map(String::from);
                if let Some(t) = target {
                    if local.contains(t.as_str()) {
                        f["ref"] = Value::String(format!("{prefix}__{t}"));
                    }
                }
            }
        }
        entities.insert(full, def);
    }
    Ok(())
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
