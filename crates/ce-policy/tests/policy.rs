use ce_auth::{Principal, Verifier};
use ce_manifest::{validate, Manifest};
use ce_policy::*;

fn load(name: &str) -> Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    validate(std::fs::read_to_string(path).unwrap().as_bytes()).expect("example is valid")
}

fn roles(r: &[&str]) -> Vec<String> {
    r.iter().map(|s| s.to_string()).collect()
}

fn principal(roles: &[&str]) -> Principal {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let list = roles
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(",");
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(
        r#"{{"sub":"u1","tenant":"11111111-1111-1111-1111-111111111111","roles":[{list}],"exp":2000}}"#
    ));
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    let token = format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()));
    Verifier::new(key)
        .unwrap()
        .verify_token(&token, 1000)
        .unwrap()
}

#[test]
fn entity_grants_and_default_deny() {
    let p = Policy::from_manifest(&load("inventory"));
    assert!(p.can_read(&roles(&["manager"]), "item"));
    assert!(p.can_write(&roles(&["manager"]), "period"));
    assert!(p.can_read(&roles(&["staff"]), "item"));
    assert!(!p.can_write(&roles(&["staff"]), "item"));
    // staff may not see entities they are not granted.
    assert!(!p.can_read(&roles(&["staff"]), "period"));
    // No roles, or an unknown role, gets nothing.
    assert!(!p.can_read(&[], "item"));
    assert!(!p.can_read(&roles(&["ghost"]), "item"));
}

#[test]
fn wildcards_cover_every_entity_and_action() {
    let p = Policy::from_manifest(&load("booking_crm"));
    let owner = roles(&["owner"]);
    assert!(p.can_read(&owner, "anything"));
    assert!(p.can_write(&owner, "anything"));
    assert!(p.can_run(&owner, "any_action"));
}

#[test]
fn write_does_not_imply_read_and_actions_do_not_need_write() {
    let p = Policy::from_manifest(&load("approval_workflow"));
    let approver = roles(&["approver"]);
    assert!(p.can_read(&approver, "request"));
    assert!(!p.can_write(&approver, "request"));
    assert!(p.can_run(&approver, "approve"));
    assert!(!p.can_run(&roles(&["requester"]), "approve"));
    let m = load("inventory");
    let inv = Policy::from_manifest(&m);
    assert!(inv.can_run(&roles(&["manager"]), "close_period"));
    assert!(!inv.can_run(&roles(&["staff"]), "close_period"));
}

#[test]
fn grants_add_up_across_roles() {
    let p = Policy::from_manifest(&load("approval_workflow"));
    let both = roles(&["requester", "approver"]);
    assert!(p.can_write(&both, "request"));
    assert!(p.can_run(&both, "reject"));
}

#[test]
fn authorize_uses_the_principals_roles() {
    let p = Policy::from_manifest(&load("approval_workflow"));
    let approver = principal(&["approver"]);
    assert!(p.authorize(&approver, Access::Read, "request").is_ok());
    assert_eq!(
        p.authorize(&approver, Access::Write, "request"),
        Err(PolicyError::Forbidden {
            what: "write request".into()
        })
    );
    assert!(p.authorize_action(&approver, "approve").is_ok());
    assert!(matches!(
        p.authorize_action(&principal(&[]), "approve"),
        Err(PolicyError::Forbidden { .. })
    ));
}
