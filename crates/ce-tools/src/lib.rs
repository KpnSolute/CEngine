//! CEngine built-in tools (draft v1). A tool is a JSON fragment of entities, state models and
//! actions that is domain-neutral: every group of businesses needs it and none owns it. A
//! template opts in by name and the fragment is merged into its manifest before validation.
//!
//! Rules, all covered by tests:
//! 1. Tools are not templates. They carry no roles, no prices, no vocabulary of one trade.
//!    The template's own `roles` decide who may read or write a tool's entities.
//! 2. Composition never overrides: an entity or action name the template already uses is an
//!    error, not a silent replacement.
//! 3. Every composed manifest is checked by `ce-manifest` like any other (callers do this),
//!    and every fragment is also valid on its own (tests).
//! 4. Tools record intent only. Delivering a notification or contacting a provider is
//!    Infajn's job; a tool never names a provider or holds credentials.

use serde_json::{json, Map, Value};

/// Tool name, source JSON. Order is the catalog order.
const TOOLS: &[(&str, &str)] = &[
    ("customers", include_str!("../tools/customers.json")),
    ("tasks", include_str!("../tools/tasks.json")),
    ("notifications", include_str!("../tools/notifications.json")),
    ("bookings", include_str!("../tools/bookings.json")),
    ("invoicing", include_str!("../tools/invoicing.json")),
    ("audit", include_str!("../tools/audit.json")),
    ("changesets", include_str!("../tools/changesets.json")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// No built-in tool has this name.
    Unknown(String),
    /// The template already defines an entity or action the tool adds.
    Clash {
        tool: String,
        kind: &'static str,
        name: String,
    },
    /// The template is not a JSON object.
    NotAnObject,
}

fn fragment(name: &str) -> Option<Value> {
    let (_, src) = TOOLS.iter().find(|(n, _)| *n == name)?;
    Some(serde_json::from_str(src).expect("built-in tools are valid JSON"))
}

/// What each built-in tool offers, for catalogs and docs.
pub fn catalog() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|(n, _)| {
            let f = fragment(n).expect("listed");
            let keys = |k: &str| -> Vec<String> {
                f[k].as_object()
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default()
            };
            json!({
                "tool": n,
                "version": f["version"],
                "title": f["title"],
                "entities": keys("entities"),
                "actions": keys("actions"),
            })
        })
        .collect()
}

/// Returns `template` with the named tools' entities and actions added. Naming a tool twice
/// adds it once. The template's roles are not touched (rule 1).
pub fn compose(template: &Value, uses: &[&str]) -> Result<Value, ToolError> {
    let mut out = template.clone();
    let root = out.as_object_mut().ok_or(ToolError::NotAnObject)?;
    let mut done: Vec<&str> = Vec::new();
    for &name in uses {
        if done.contains(&name) {
            continue;
        }
        done.push(name);
        let f = fragment(name).ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        for (section, kind) in [("entities", "entity"), ("actions", "action")] {
            let Some(add) = f[section].as_object() else {
                continue;
            };
            let slot = root
                .entry(section)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or(ToolError::NotAnObject)?;
            for (k, v) in add {
                if slot.contains_key(k) {
                    return Err(ToolError::Clash {
                        tool: name.to_string(),
                        kind,
                        name: k.clone(),
                    });
                }
                slot.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(out)
}
