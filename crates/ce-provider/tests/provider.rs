use ce_auth::{TenantScope, Verifier};
use ce_provider::*;
use serde_json::json;
use std::sync::Mutex;

fn scope() -> TenantScope {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p =
        B64.encode(r#"{"sub":"u1","tenant":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa","exp":2000}"#);
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
    Verifier::new(key)
        .unwrap()
        .verify_token(&token, 1000)
        .unwrap()
        .scope()
        .clone()
}

/// Records what reached it and answers with the tenant it was called for.
#[derive(Default)]
struct Spy(Mutex<Vec<String>>);
impl Provider for Spy {
    fn call(
        &self,
        scope: &TenantScope,
        req: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.0.lock().unwrap().push(req.operation.clone());
        Ok(ProviderResponse {
            data: json!({ "tenant": scope.tenant_id() }),
        })
    }
}

#[test]
fn connector_names_round_trip_and_cover_the_spec() {
    assert_eq!(ConnectorType::all().count(), 19);
    for t in ConnectorType::all() {
        assert_eq!(ConnectorType::parse(t.name()), Some(t));
    }
    assert_eq!(ConnectorType::parse("stripe"), None);
    assert!(ConnectorType::Banking.reads_only_first());
    assert!(!ConnectorType::Messaging.reads_only_first());
}

#[test]
fn the_noop_provider_refuses_everything() {
    let req = ProviderRequest::read(ConnectorType::Maps, "geocode.lookup", json!({}));
    assert_eq!(
        NoopProvider.call(&scope(), &req),
        Err(ProviderError::Unavailable)
    );
}

#[test]
fn operation_names_are_checked() {
    for ok in ["balances.list", "a.b.c.d", "email.send_batch"] {
        assert!(valid_operation(ok), "{ok}");
    }
    for bad in [
        "balances",
        "a.b.c.d.e",
        "Email.send",
        "email..send",
        "email.1x",
        "",
    ] {
        assert!(!valid_operation(bad), "{bad}");
    }
    let guard = ReadFirst::new(Spy::default());
    let req = ProviderRequest::read(ConnectorType::Banking, "Balances", json!({}));
    assert!(matches!(
        guard.call(&scope(), &req),
        Err(ProviderError::Invalid(_))
    ));
}

#[test]
fn reads_pass_through_with_the_callers_tenant() {
    let guard = ReadFirst::new(Spy::default());
    let req = ProviderRequest::read(ConnectorType::Banking, "balances.list", json!({}));
    let out = guard.call(&scope(), &req).unwrap();
    assert_eq!(out.data["tenant"], "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
}

#[test]
fn money_writes_need_an_approval_and_every_write_needs_a_key() {
    let guard = ReadFirst::new(Spy::default());
    let pay = ProviderRequest::write(ConnectorType::Payments, "charges.create", json!({}), "k1");
    assert_eq!(
        guard.call(&scope(), &pay),
        Err(ProviderError::ApprovalRequired)
    );
    assert!(guard
        .call(&scope(), &pay.clone().approved_by("approval-7"))
        .is_ok());

    // Other connectors need no approval, but still a key.
    let mail = ProviderRequest::write(ConnectorType::Messaging, "email.send", json!({}), "k2");
    assert!(guard.call(&scope(), &mail).is_ok());
    let mut keyless = mail.clone();
    keyless.idempotency_key = None;
    assert!(matches!(
        guard.call(&scope(), &keyless),
        Err(ProviderError::Invalid(_))
    ));
    keyless.idempotency_key = Some(String::new());
    assert!(matches!(
        guard.call(&scope(), &keyless),
        Err(ProviderError::Invalid(_))
    ));

    // Refused calls never reach the inner provider.
    let spy = std::sync::Arc::new(Spy::default());
    let guard = ReadFirst::new(spy.clone());
    assert!(guard.call(&scope(), &pay).is_err());
    assert!(guard.call(&scope(), &pay.clone().approved_by("")).is_err());
    assert!(spy.0.lock().unwrap().is_empty());
    assert!(guard.call(&scope(), &pay.approved_by("approval-7")).is_ok());
    assert_eq!(spy.0.lock().unwrap().len(), 1);
}
