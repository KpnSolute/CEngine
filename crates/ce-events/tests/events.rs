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
        name: "request.approved".into(),
        version: 1,
        subject: Some("rec-1".into()),
        data: json!({"n": n}),
    }
}

#[test]
fn type_names_and_emit_validation() {
    assert_eq!(
        event_type("request.approved", 1),
        "com.kpnsolute.compute.request.approved.v1"
    );
    assert_eq!(
        event_type("inventory.period.closed", 2),
        "com.kpnsolute.compute.inventory.period.closed.v2"
    );
    for ok in [
        "request.approved",
        "inventory.period.closed",
        "a.b.c.d",
        "a_1.b2",
    ] {
        assert!(valid_event_name(ok), "{ok}");
    }
    for bad in [
        "",
        "request",
        "a.b.c.d.e",
        ".x",
        "x.",
        "A.b",
        "a..b",
        "1a.b",
        "a.b-c",
    ] {
        assert!(!valid_event_name(bad), "{bad}");
    }
}

#[test]
fn time_is_an_rfc3339_string() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // leap day
    assert_eq!(rfc3339(1_791_660_600), "2026-10-10T19:30:00Z");
    assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
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
    assert_eq!(v["time"], "1970-01-01T00:01:40Z");
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

#[test]
fn a_relay_that_crashes_every_time_cannot_pin_an_event_forever() {
    let o = MemoryOutbox::new();
    o.enqueue(&scope(A), "e1", ev(1), 0).unwrap();
    o.enqueue(&scope(A), "e2", ev(2), 0).unwrap();
    let mut now = 0;
    // Claim without ever acking or failing; each lease expires.
    for attempt in 1..=MAX_ATTEMPTS {
        let got = o.claim(1, now, 30);
        assert_eq!(got[0].event.id, "e1");
        assert_eq!(got[0].attempt, attempt);
        now += 31;
    }
    // The next claim dead-letters e1 instead of handing it out a sixth time, and e2 proceeds.
    let got = o.claim(1, now, 30);
    assert_eq!(o.status(A, "e1"), Some(Status::DeadLettered));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].event.id, "e2");
}

#[test]
fn two_quick_rotations_keep_both_old_keys_valid_for_their_window() {
    let mut s = Signer::new("k1", b"0123456789abcdef0123456789abcdef").unwrap();
    let (_, sig1) = s.sign(b"x");
    s.rotate("k2", b"fedcba9876543210fedcba9876543210", 1000)
        .unwrap();
    let (_, sig2) = s.sign(b"x");
    s.rotate("k3", b"00112233445566778899aabbccddeeff", 1100)
        .unwrap();
    // k1 was rotated out at 1000, k2 at 1100; both still verify at 1200.
    assert!(s.verify("k1", b"x", &sig1, 1200).is_ok());
    assert!(s.verify("k2", b"x", &sig2, 1200).is_ok());
    // k1 expires at 1600, k2 at 1700.
    assert!(s.verify("k1", b"x", &sig1, 1600).is_err());
    assert!(s.verify("k2", b"x", &sig2, 1699).is_ok());
    assert!(s.verify("k2", b"x", &sig2, 1700).is_err());
}

#[test]
fn every_emit_in_the_example_manifests_is_a_valid_event_name() {
    let dir = format!("{}/../../spec/examples", env!("CARGO_MANIFEST_DIR"));
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        for action in v["actions"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, a)| a)
        {
            for effect in action["effect"].as_array().into_iter().flatten() {
                if let Some(name) = effect["emit"].as_str() {
                    assert!(valid_event_name(name), "{name}");
                    seen += 1;
                }
            }
        }
    }
    assert!(
        seen >= 3,
        "expected the examples to emit events, saw {seen}"
    );
}
