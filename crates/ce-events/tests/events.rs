use ce_auth::{TenantScope, Verifier};
use ce_events::*;
use serde_json::json;

const A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

fn scope(tenant: &str) -> TenantScope {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let key = b"0123456789abcdef0123456789abcdef";
    let h = B64.encode(r#"{"alg":"HS256"}"#);
    let p = B64.encode(format!(r#"{{"sub":"u1","tenant":"{tenant}","exp":2000}}"#));
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

fn ev(n: i64) -> NewEvent {
    NewEvent {
        entity: "request".into(),
        verb: "approved".into(),
        version: 1,
        subject: Some("rec-1".into()),
        data: json!({"n": n}),
    }
}

#[test]
fn type_names_and_emit_parsing() {
    assert_eq!(
        event_type("request", "approved", 1),
        "com.kpnsolute.compute.request.approved.v1"
    );
    assert_eq!(
        parse_emit("request.approved"),
        Some(("request".into(), "approved".into()))
    );
    for bad in ["", "request", "a.b.c", ".x", "x."] {
        assert_eq!(parse_emit(bad), None, "{bad}");
    }
}

#[test]
fn cloud_event_has_the_required_attributes_and_tenant_from_scope() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 100).unwrap();
    let c = &o.claim(10, 100, 30)[0];
    let v = c.event.to_cloud_event();
    assert_eq!(v["specversion"], "1.0");
    assert_eq!(v["id"], "e1");
    assert_eq!(v["type"], "com.kpnsolute.compute.request.approved.v1");
    assert_eq!(v["tenant"], A);
    assert_eq!(v["subject"], "rec-1");
    assert_eq!(v["data"], json!({"n": 1}));
    assert_eq!(c.attempt, 1);
}

#[test]
fn duplicate_ids_are_refused_per_tenant_only() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 0).unwrap();
    assert_eq!(
        o.enqueue(&scope(A), "e1", ev(1), 0),
        Err(OutboxError::Duplicate)
    );
    o.enqueue(&scope(B), "e1", ev(1), 0).unwrap();
}

#[test]
fn delivery_is_in_order_per_tenant_and_waits_for_the_earlier_event() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "a1", ev(1), 0).unwrap();
    o.enqueue(&scope(A), "a2", ev(2), 0).unwrap();
    o.enqueue(&scope(B), "b1", ev(3), 0).unwrap();
    // One per tenant at a time: a1 and b1, not a2.
    let first = o.claim(10, 0, 30);
    let ids: Vec<_> = first.iter().map(|c| c.event.id.as_str()).collect();
    assert_eq!(ids, ["a1", "b1"]);
    // a2 stays blocked while a1 is leased.
    assert!(o.claim(10, 1, 30).is_empty());
    o.ack(A, "a1").unwrap();
    let next = o.claim(10, 2, 30);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].event.id, "a2");
}

#[test]
fn failure_backs_off_then_dead_letters() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 0).unwrap();
    let mut now = 0;
    for attempt in 1..=MAX_ATTEMPTS {
        let got = o.claim(1, now, 30);
        assert_eq!(got.len(), 1, "attempt {attempt}");
        assert_eq!(got[0].attempt, attempt);
        let status = o.fail(A, "e1", now).unwrap();
        if attempt < MAX_ATTEMPTS {
            assert_eq!(status, Status::Pending);
            // Not due immediately after a failure.
            assert!(o.claim(1, now, 30).is_empty());
            now += 4000; // past any backoff
        } else {
            assert_eq!(status, Status::DeadLettered);
        }
    }
    assert!(o.claim(1, now + 100_000, 30).is_empty());
    assert_eq!(o.status(A, "e1"), Some(Status::DeadLettered));
}

#[test]
fn an_expired_lease_is_handed_out_again() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 0).unwrap();
    assert_eq!(o.claim(1, 0, 30).len(), 1);
    assert!(o.claim(1, 29, 30).is_empty());
    let again = o.claim(1, 30, 30);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].attempt, 2);
}

#[test]
fn ack_and_fail_need_an_in_flight_event() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 0).unwrap();
    assert_eq!(o.ack(A, "e1"), Err(OutboxError::NotInFlight));
    assert_eq!(o.ack(A, "ghost"), Err(OutboxError::NotFound));
    // Another tenant's id is not found.
    o.claim(1, 0, 30);
    assert_eq!(o.ack(B, "e1"), Err(OutboxError::NotFound));
    o.ack(A, "e1").unwrap();
    assert_eq!(o.fail(A, "e1", 0), Err(OutboxError::NotInFlight));
}

#[test]
fn signatures_verify_and_reject_tampering() {
    let s = Signer::new("k1", b"0123456789abcdef0123456789abcdef").unwrap();
    let payload = b"{\"a\":1}";
    let (kid, sig) = s.sign(payload);
    assert_eq!(kid, "k1");
    assert!(s.verify(&kid, payload, &sig, 0).is_ok());
    assert_eq!(
        s.verify(&kid, b"{\"a\":2}", &sig, 0),
        Err(SignError::BadSignature)
    );
    assert_eq!(
        s.verify("nope", payload, &sig, 0),
        Err(SignError::BadSignature)
    );
    assert_eq!(Signer::new("k", b"short").err(), Some(SignError::ShortKey));
}

#[test]
fn rotated_keys_overlap_for_ten_minutes() {
    let mut s = Signer::new("k1", b"0123456789abcdef0123456789abcdef").unwrap();
    let payload = b"x";
    let (_, old_sig) = s.sign(payload);
    s.rotate("k2", b"fedcba9876543210fedcba9876543210", 1000)
        .unwrap();
    let (kid, new_sig) = s.sign(payload);
    assert_eq!(kid, "k2");
    assert!(s.verify("k2", payload, &new_sig, 1000).is_ok());
    // The old key still verifies inside the window and not after.
    assert!(s
        .verify("k1", payload, &old_sig, 1000 + KEY_OVERLAP_SECS - 1)
        .is_ok());
    assert_eq!(
        s.verify("k1", payload, &old_sig, 1000 + KEY_OVERLAP_SECS),
        Err(SignError::BadSignature)
    );
}
