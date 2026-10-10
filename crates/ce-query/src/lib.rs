//! CEngine query planner (draft v1). Turns a list request into parameterised SQL.
//!
//! Safety properties, all covered by tests:
//! 1. Every statement is `WHERE "tenant_id" = $1 AND ...`; the caller's filter sits inside
//!    its own parentheses so `or` cannot escape the tenant predicate.
//! 2. Identifiers come only from the validated manifest, never from the request.
//! 3. Every value is a bound parameter; none is spliced into SQL text.
//! 4. Only declared, indexed fields can be sorted; page size is capped.
//!
//! `TenantScope::new` is the only way to build a scope. Until the auth crate exists it is
//! public; when `verify_token` lands, the constructor becomes private to that crate.

mod filter;
mod plan;

pub use plan::{
    plan_list, ListRequest, Param, Planned, QueryError, SortDir, SortKey, Target, DEFAULT_LIMIT,
    MAX_LIMIT,
};

/// Proof that a request is scoped to one tenant.
#[derive(Debug, Clone)]
pub struct TenantScope {
    tenant_id: String,
}

impl TenantScope {
    pub fn new(tenant_id: impl Into<String>) -> Self {
        TenantScope {
            tenant_id: tenant_id.into(),
        }
    }
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }
}
