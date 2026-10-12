use std::collections::BTreeSet;

use semver::{Version, VersionReq};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{Module, Registry, KIND_POLICY_PACK, SCHEMA_VERSION};

pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_POLICIES: usize = 64;
const MAX_RULE: usize = 2000;
const MAX_LIST: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    schema_version: u32,
    plugin: RawPlugin,
    compatibility: RawCompat,
    execution: RawExec,
    #[serde(default)]
    capabilities: RawCaps,
    #[serde(default)]
    policy: Vec<RawPolicy>,
    #[serde(default)]
    tests: Option<RawTests>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    id: String,
    version: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompat {
    engine: String,
    module: String,
    module_api: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExec {
    kind: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCaps {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
    #[serde(default)]
    emit: Vec<String>,
    #[serde(default)]
    external_network: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    slot: String,
    #[serde(default)]
    rule: Option<String>,
    #[serde(default)]
    params: toml::Table,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTests {
    fixtures: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub emit: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub slot: String,
    pub rule: Option<String>,
    pub params: Map<String, Value>,
}

/// A validated plugin descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub id: String,
    pub version: Version,
    pub description: String,
    pub module: String,
    pub module_api: u32,
    pub engine: VersionReq,
    pub capabilities: Capabilities,
    pub policies: Vec<Policy>,
    /// Relative, normalized path of the synthetic fixtures folder, when declared.
    pub fixtures: Option<String>,
}

/// One reason a descriptor was refused. `path` names the offending field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: &'static str,
    pub path: String,
    pub message: String,
}

fn v(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Violation {
    Violation {
        code,
        path: path.into(),
        message: message.into(),
    }
}

/// Read and validate a `plugin.toml` against the registry and the running engine version.
/// Collects every violation it can; returns `Err` with at least one.
pub fn parse(text: &str, registry: &Registry, engine: &Version) -> Result<Plugin, Vec<Violation>> {
    if text.len() > MAX_BYTES {
        return Err(vec![v(
            "too_large",
            "",
            format!("descriptor exceeds {MAX_BYTES} bytes"),
        )]);
    }
    let raw: Raw =
        toml::from_str(text).map_err(|e| vec![v("malformed", "", e.message().to_string())])?;
    let mut bad = Vec::new();

    if raw.schema_version != SCHEMA_VERSION {
        bad.push(v(
            "unsupported_schema",
            "schema_version",
            format!("only version {SCHEMA_VERSION} is read"),
        ));
    }
    if !slug(&raw.plugin.id) {
        bad.push(v(
            "bad_id",
            "plugin.id",
            "lowercase letters, digits and hyphens, starting with a letter, up to 63",
        ));
    }
    let version = match Version::parse(&raw.plugin.version) {
        Ok(ver) => Some(ver),
        Err(_) => {
            bad.push(v(
                "bad_version",
                "plugin.version",
                "must be a semantic version",
            ));
            None
        }
    };
    let req = match VersionReq::parse(&raw.compatibility.engine) {
        Ok(req) => {
            if !req.matches(engine) {
                bad.push(v(
                    "engine_incompatible",
                    "compatibility.engine",
                    format!("engine {engine} does not satisfy {req}"),
                ));
            }
            Some(req)
        }
        Err(_) => {
            bad.push(v(
                "bad_engine_range",
                "compatibility.engine",
                "must be a semantic version range",
            ));
            None
        }
    };
    if raw.execution.kind != KIND_POLICY_PACK {
        bad.push(v(
            "unsupported_host",
            "execution.kind",
            format!(
                "only \"{KIND_POLICY_PACK}\" is supported, got \"{}\"",
                raw.execution.kind
            ),
        ));
    }
    if raw.capabilities.external_network {
        bad.push(v(
            "network_denied",
            "capabilities.external_network",
            "plugins never reach the network; Infajn does",
        ));
    }

    let module = registry.module(&raw.compatibility.module);
    match module {
        None => bad.push(v(
            "unknown_module",
            "compatibility.module",
            "module is not registered",
        )),
        Some(m) if m.api != raw.compatibility.module_api => bad.push(v(
            "module_api_mismatch",
            "compatibility.module_api",
            format!(
                "module {} speaks API {}, plugin asks for {}",
                m.id, m.api, raw.compatibility.module_api
            ),
        )),
        Some(_) => {}
    }

    capability_check(&mut bad, module, &raw.capabilities);
    let policies = policy_check(&mut bad, module, &raw.policy);
    let fixtures = raw
        .tests
        .as_ref()
        .and_then(|t| match relative_path(&t.fixtures) {
            Some(p) => Some(p),
            None => {
                bad.push(v(
                    "bad_path",
                    "tests.fixtures",
                    "must be a relative path inside the plugin folder",
                ));
                None
            }
        });

    match (bad.is_empty(), version, req) {
        (true, Some(version), Some(engine)) => Ok(Plugin {
            id: raw.plugin.id,
            version,
            description: raw.plugin.description,
            module: raw.compatibility.module,
            module_api: raw.compatibility.module_api,
            engine,
            capabilities: Capabilities {
                read: raw.capabilities.read,
                write: raw.capabilities.write,
                emit: raw.capabilities.emit,
            },
            policies,
            fixtures,
        }),
        _ => Err(bad),
    }
}

type Group<'a> = (&'a str, &'a Vec<String>, Option<&'a Vec<String>>);

fn capability_check(bad: &mut Vec<Violation>, module: Option<&Module>, caps: &RawCaps) {
    let groups: [Group; 3] = [
        ("read", &caps.read, module.map(|m| &m.read)),
        ("write", &caps.write, module.map(|m| &m.write)),
        ("emit", &caps.emit, module.map(|m| &m.emit)),
    ];
    for (name, asked, allowed) in groups {
        if asked.len() > MAX_LIST {
            bad.push(v(
                "too_many",
                format!("capabilities.{name}"),
                format!("at most {MAX_LIST} entries"),
            ));
            continue;
        }
        let mut seen = BTreeSet::new();
        for cap in asked {
            if !seen.insert(cap) {
                bad.push(v(
                    "duplicate",
                    format!("capabilities.{name}"),
                    format!("\"{cap}\" listed twice"),
                ));
            }
            if let Some(allowed) = allowed {
                if !allowed.contains(cap) {
                    bad.push(v(
                        "capability_denied",
                        format!("capabilities.{name}"),
                        format!("\"{cap}\" is not offered by the module"),
                    ));
                }
            }
        }
    }
}

fn policy_check(
    bad: &mut Vec<Violation>,
    module: Option<&Module>,
    raw: &[RawPolicy],
) -> Vec<Policy> {
    if raw.len() > MAX_POLICIES {
        bad.push(v(
            "too_many",
            "policy",
            format!("at most {MAX_POLICIES} policies"),
        ));
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, p) in raw.iter().enumerate() {
        let at = format!("policy[{i}]");
        if !seen.insert(p.slot.as_str()) {
            bad.push(v(
                "duplicate_slot",
                format!("{at}.slot"),
                format!("slot \"{}\" filled twice", p.slot),
            ));
        }
        let slot = module.and_then(|m| m.slots.iter().find(|s| s.name == p.slot));
        if module.is_some() && slot.is_none() {
            bad.push(v(
                "unknown_slot",
                format!("{at}.slot"),
                format!("module has no slot \"{}\"", p.slot),
            ));
        }
        if let Some(slot) = slot {
            match (slot.rule, p.rule.is_some()) {
                (Some(true), false) => bad.push(v(
                    "rule_required",
                    format!("{at}.rule"),
                    "this slot needs a rule",
                )),
                (Some(false), true) => bad.push(v(
                    "rule_forbidden",
                    format!("{at}.rule"),
                    "this slot takes parameters only",
                )),
                _ => {}
            }
            for key in p.params.keys() {
                if !slot.params.contains(key) {
                    bad.push(v(
                        "unknown_param",
                        format!("{at}.params.{key}"),
                        "slot does not accept this parameter",
                    ));
                }
            }
        }
        if let Some(rule) = &p.rule {
            if rule.chars().count() > MAX_RULE {
                bad.push(v(
                    "rule_too_long",
                    format!("{at}.rule"),
                    format!("at most {MAX_RULE} characters"),
                ));
            } else if let Err(e) = ce_expr::parse(rule) {
                bad.push(v("bad_rule", format!("{at}.rule"), format!("{e:?}")));
            }
        }
        let mut params = Map::new();
        for (k, val) in &p.params {
            match json(val) {
                Some(j) => {
                    params.insert(k.clone(), j);
                }
                None => bad.push(v(
                    "bad_param",
                    format!("{at}.params.{k}"),
                    "use strings, integers, booleans, arrays or tables; write decimals as strings",
                )),
            }
        }
        out.push(Policy {
            slot: p.slot.clone(),
            rule: p.rule.clone(),
            params,
        });
    }
    out
}

/// TOML to JSON for parameters. Floats and datetimes are refused: money and quantities are
/// decimal strings or integers, so a policy can never depend on binary rounding.
fn json(val: &toml::Value) -> Option<Value> {
    Some(match val {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Array(a) => Value::Array(a.iter().map(json).collect::<Option<_>>()?),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, x)| json(x).map(|j| (k.clone(), j)))
                .collect::<Option<_>>()?,
        ),
        toml::Value::Float(_) | toml::Value::Datetime(_) => return None,
    })
}

fn slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A path is accepted only when it is relative, has no `..`, `.`-only or empty parts, no
/// backslashes and no drive prefix. Returns the normalized form.
fn relative_path(p: &str) -> Option<String> {
    if p.is_empty()
        || p.len() > 200
        || p.starts_with('/')
        || p.contains('\\')
        || p.contains('\0')
        || p.contains(':')
    {
        return None;
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    Some(parts.join("/"))
}
