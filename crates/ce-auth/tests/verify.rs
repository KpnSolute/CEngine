use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ce_auth::{AuthError, Verifier};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const TENANT: &str = "11111111-1111-1111-1111-111111111111";

fn sign(key: &[u8], header: &str, claims: &str) -> String {
    let h = B64.encode(header);
    let p = B64.encode(claims);
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(format!("{h}.{p}").as_bytes());
    format!("{h}.{p}.{}", B64.encode(mac.finalize().into_bytes()))
}

fn claims(extra: &str) -> String {
    format!(r#"{{"sub":"u1","tenant":"{TENANT}","roles":["admin"],"exp":2000{extra}}}"#)
}

fn v() -> Verifier {
    Verifier::new(KEY).unwrap()
}

#[test]
fn valid_token_yields_scope_and_roles() {
    let t = sign(KEY, r#"{"alg":"HS256","typ":"JWT"}"#, &claims(""));
    let p = v().verify_token(&t, 1000).unwrap();
    assert_eq!(p.scope().tenant_id(), TENANT);
    assert_eq!(p.user_id, "u1");
    assert!(p.has_role("admin") && !p.has_role("owner"));
}

#[test]
fn expiry_and_not_before() {
    let t = sign(KEY, r#"{"alg":"HS256"}"#, &claims(""));
    assert_eq!(v().verify_token(&t, 2000).unwrap_err(), AuthError::Expired);
    let t = sign(KEY, r#"{"alg":"HS256"}"#, &claims(r#","nbf":1500"#));
    assert_eq!(
        v().verify_token(&t, 1000).unwrap_err(),
        AuthError::NotYetValid
    );
    assert!(v().verify_token(&t, 1500).is_ok());
}

#[test]
fn rejects_wrong_key_tampering_and_other_algorithms() {
    let other = b"ffffffffffffffffffffffffffffffff";
    let t = sign(other, r#"{"alg":"HS256"}"#, &claims(""));
    assert_eq!(
        v().verify_token(&t, 1000).unwrap_err(),
        AuthError::BadSignature
    );

    // Swap the tenant after signing.
    let good = sign(KEY, r#"{"alg":"HS256"}"#, &claims(""));
    let mut parts: Vec<&str> = good.split('.').collect();
    let forged = B64.encode(claims("").replace(TENANT, "22222222-2222-2222-2222-222222222222"));
    parts[1] = &forged;
    assert_eq!(
        v().verify_token(&parts.join("."), 1000).unwrap_err(),
        AuthError::BadSignature
    );

    for alg in ["none", "HS512", "RS256"] {
        let t = sign(KEY, &format!(r#"{{"alg":"{alg}"}}"#), &claims(""));
        assert_eq!(
            v().verify_token(&t, 1000).unwrap_err(),
            AuthError::BadAlgorithm
        );
    }
    let unsigned = format!(
        "{}.{}.",
        B64.encode(r#"{"alg":"none"}"#),
        B64.encode(claims(""))
    );
    assert!(v().verify_token(&unsigned, 1000).is_err());
}

#[test]
fn rejects_malformed_and_bad_claims() {
    for t in ["", "a.b", "a.b.c.d", "!!.!!.!!"] {
        assert!(v().verify_token(t, 1000).is_err(), "{t}");
    }
    let bad = [
        claims("").replace(TENANT, "not-a-uuid"),
        claims("").replace(TENANT, "11111111-1111-1111-1111-11111111111G"),
        claims("").replace(TENANT, "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA"),
        claims("").replace(r#""u1""#, r#""""#),
    ];
    for c in bad {
        let t = sign(KEY, r#"{"alg":"HS256"}"#, &c);
        assert_eq!(
            v().verify_token(&t, 1000).unwrap_err(),
            AuthError::BadClaims,
            "{c}"
        );
    }
}

#[test]
fn short_keys_are_refused() {
    assert!(Verifier::new(b"short").is_err());
}
