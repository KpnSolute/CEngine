//! CEngine roles (draft v1). Entity-level only: a role grants `read` and `write` on named
//! entities and the right to run named actions. `*` means every entity or action.
//! Field-level rules are not part of 0.1.
//!
//! Rules:
//! - Default deny. No role, an unknown role, or a role that does not list the entity gives no access.
//! - Grants add up across a principal's roles; nothing subtracts.
//! - `write` does not imply `read`. A role that writes must also list the entity under `read`
//!   if it should see it.
//! - Running an action needs the action in `actions`. It does not need `write`, because an
//!   action is the controlled write path (its effects are fixed by the manifest).
//! - The caller still applies the tenant scope; this crate decides nothing about tenants.

use ce_auth::Principal;
use ce_manifest::Manifest;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// The principal has no role granting this. Maps to the `forbidden` error code.
    Forbidden { what: String },
}

#[derive(Default)]
struct Grants {
    read: HashSet<String>,
    write: HashSet<String>,
    actions: HashSet<String>,
}

pub struct Policy {
    roles: std::collections::HashMap<String, Grants>,
}

fn set(v: &Value) -> HashSet<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(String::from))
        .collect()
}

fn has(set: &HashSet<String>, name: &str) -> bool {
    set.contains("*") || set.contains(name)
}

impl Policy {
    pub fn from_manifest(m: &Manifest) -> Policy {
        let roles = m.value["roles"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, r)| {
                (
                    name.clone(),
                    Grants {
                        read: set(&r["read"]),
                        write: set(&r["write"]),
                        actions: set(&r["actions"]),
                    },
                )
            })
            .collect();
        Policy { roles }
    }

    fn any(&self, roles: &[String], f: impl Fn(&Grants) -> bool) -> bool {
        roles.iter().filter_map(|r| self.roles.get(r)).any(f)
    }

    pub fn can_read(&self, roles: &[String], entity: &str) -> bool {
        self.any(roles, |g| has(&g.read, entity))
    }

    pub fn can_write(&self, roles: &[String], entity: &str) -> bool {
        self.any(roles, |g| has(&g.write, entity))
    }

    pub fn can_run(&self, roles: &[String], action: &str) -> bool {
        self.any(roles, |g| has(&g.actions, action))
    }

    pub fn authorize(
        &self,
        who: &Principal,
        access: Access,
        entity: &str,
    ) -> Result<(), PolicyError> {
        let ok = match access {
            Access::Read => self.can_read(&who.roles, entity),
            Access::Write => self.can_write(&who.roles, entity),
        };
        if ok {
            Ok(())
        } else {
            Err(PolicyError::Forbidden {
                what: format!("{access:?} {entity}").to_lowercase(),
            })
        }
    }

    pub fn authorize_action(&self, who: &Principal, action: &str) -> Result<(), PolicyError> {
        if self.can_run(&who.roles, action) {
            Ok(())
        } else {
            Err(PolicyError::Forbidden {
                what: format!("run {action}"),
            })
        }
    }
}
