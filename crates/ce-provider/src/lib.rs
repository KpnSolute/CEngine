//! CEngine provider contract (draft v1): how CEngine asks Infajn for anything outside the template.
//!
//! CEngine is a controller. Infajn makes every provider call (including AI models) and CEngine
//! uses the result, so nothing here names a provider or holds a credential. A request carries
//! only a connector type, an operation, parameters and an idempotency key. The tenant comes from
//! the `TenantScope` passed to `call`, never from the request.
//!
//! - [`ConnectorType`]: the connector types from `spec/groups-and-connectors.md`.
//! - [`Provider`]: the trait Infajn's client implements. [`NoopProvider`] is the local default:
//!   it refuses every call with `Unavailable`, so a template without Infajn fails loudly.
//! - [`ReadFirst`]: a wrapper that refuses writes to money-moving connectors (banking, payments,
//!   HR and payroll) unless the request names an approval, and refuses every write that does not
//!   carry an idempotency key. Rules D.3 and D.5.

use ce_auth::TenantScope;
use serde_json::Value;

/// The connector types a template may declare. Direction is advisory; see [`ConnectorType::reads_only_first`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectorType {
    Auth,
    Storage,
    Banking,
    Payments,
    Accounting,
    Messaging,
    Calendar,
    Crm,
    Commerce,
    Erp,
    HrPayroll,
    Logistics,
    Maps,
    Documents,
    Analytics,
    Ai,
    Kyc,
    Devices,
    Generic,
}

const ALL: [(ConnectorType, &str); 19] = [
    (ConnectorType::Auth, "auth"),
    (ConnectorType::Storage, "storage"),
    (ConnectorType::Banking, "banking"),
    (ConnectorType::Payments, "payments"),
    (ConnectorType::Accounting, "accounting"),
    (ConnectorType::Messaging, "messaging"),
    (ConnectorType::Calendar, "calendar"),
    (ConnectorType::Crm, "crm"),
    (ConnectorType::Commerce, "commerce"),
    (ConnectorType::Erp, "erp"),
    (ConnectorType::HrPayroll, "hr_payroll"),
    (ConnectorType::Logistics, "logistics"),
    (ConnectorType::Maps, "maps"),
    (ConnectorType::Documents, "documents"),
    (ConnectorType::Analytics, "analytics"),
    (ConnectorType::Ai, "ai"),
    (ConnectorType::Kyc, "kyc"),
    (ConnectorType::Devices, "devices"),
    (ConnectorType::Generic, "generic"),
];

impl ConnectorType {
    /// Every connector type, in spec order.
    pub fn all() -> impl Iterator<Item = ConnectorType> {
        ALL.iter().map(|(t, _)| *t)
    }

    /// The stable name used in manifests and on the wire.
    pub fn name(self) -> &'static str {
        ALL.iter()
            .find(|(t, _)| *t == self)
            .map(|(_, n)| *n)
            .unwrap_or("")
    }

    pub fn parse(name: &str) -> Option<ConnectorType> {
        ALL.iter().find(|(_, n)| *n == name).map(|(t, _)| *t)
    }

    /// Banking, payments and HR/payroll move money: writes need a separate approval (rules D.3, D.5).
    pub fn reads_only_first(self) -> bool {
        matches!(
            self,
            ConnectorType::Banking | ConnectorType::Payments | ConnectorType::HrPayroll
        )
    }
}

/// Whether a call only reads from the outside world or changes something there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    pub connector: ConnectorType,
    /// A dotted operation name such as `balances.list` or `email.send`. Its meaning is the connector contract's.
    pub operation: String,
    pub access: Access,
    pub params: Value,
    /// Required on writes so a retry never repeats the effect. Reads may omit it.
    pub idempotency_key: Option<String>,
    /// A reference to the approval that allows a write to a money-moving connector. Never a credential.
    pub approval: Option<String>,
}

impl ProviderRequest {
    pub fn read(connector: ConnectorType, operation: &str, params: Value) -> Self {
        ProviderRequest {
            connector,
            operation: operation.to_string(),
            access: Access::Read,
            params,
            idempotency_key: None,
            approval: None,
        }
    }

    pub fn write(
        connector: ConnectorType,
        operation: &str,
        params: Value,
        idempotency_key: &str,
    ) -> Self {
        ProviderRequest {
            connector,
            operation: operation.to_string(),
            access: Access::Write,
            params,
            idempotency_key: Some(idempotency_key.to_string()),
            approval: None,
        }
    }

    pub fn approved_by(mut self, approval: &str) -> Self {
        self.approval = Some(approval.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// No provider is attached (the local default), or Infajn cannot be reached.
    Unavailable,
    /// The request is malformed: bad operation name, or a write without an idempotency key.
    Invalid(&'static str),
    /// A write to a read-first connector without an approval.
    ApprovalRequired,
    /// The provider answered with a failure the template may show to its users.
    Failed(String),
}

/// Checks an operation name: 2 to 4 dot-separated segments, each `[a-z][a-z0-9_]*`.
pub fn valid_operation(op: &str) -> bool {
    let parts: Vec<&str> = op.split('.').collect();
    (2..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            let b = p.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_lowercase()
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        })
}

/// What Infajn's client implements. The scope says whose data this is.
pub trait Provider: Send + Sync {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError>;
}

impl<T: Provider + ?Sized> Provider for std::sync::Arc<T> {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        (**self).call(scope, req)
    }
}

/// The local default: no provider is attached, so every call is `Unavailable`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProvider;

impl Provider for NoopProvider {
    fn call(
        &self,
        _scope: &TenantScope,
        _req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::Unavailable)
    }
}

/// Enforces the request rules before anything reaches the inner provider.
pub struct ReadFirst<P: Provider> {
    inner: P,
}

impl<P: Provider> ReadFirst<P> {
    pub fn new(inner: P) -> Self {
        ReadFirst { inner }
    }
}

impl<P: Provider> Provider for ReadFirst<P> {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        if !valid_operation(&req.operation) {
            return Err(ProviderError::Invalid(
                "operation must be 2 to 4 dotted lower-case segments",
            ));
        }
        if req.access == Access::Write {
            if req.idempotency_key.as_deref().is_none_or(str::is_empty) {
                return Err(ProviderError::Invalid("a write needs an idempotency key"));
            }
            if req.connector.reads_only_first() && req.approval.as_deref().is_none_or(str::is_empty)
            {
                return Err(ProviderError::ApprovalRequired);
            }
        }
        self.inner.call(scope, req)
    }
}
