use ce_auth::Verifier;
use ce_provider::{ConnectorType, Provider, ProviderError, ProviderRequest};
use ce_provider_http::HttpProvider;
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn scope() -> ce_auth::TenantScope {
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

/// Answers one request with `status` and `body`; returns the raw request it received.
fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/provider",
        l.local_addr().unwrap().port()
    );
    let (status, body) = (status.to_string(), body.to_string());
    let h = std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = c.read(&mut buf).unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some(i) = text.find("\r\n\r\n") {
                let len = text[..i]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if got.len() >= i + 4 + len {
                    break;
                }
            }
        }
        write!(c, "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
        String::from_utf8_lossy(&got).to_string()
    });
    (url, h)
}

fn req() -> ProviderRequest {
    ProviderRequest::read(ConnectorType::Ai, "records.extract", json!({"text": "hi"}))
}

#[test]
fn sends_the_request_with_the_token_and_returns_data() {
    let (url, h) = serve_once("200 OK", r#"{"data":{"records":[{"a":1}]}}"#);
    let p = HttpProvider::new(&url, "s3cret", Duration::from_secs(5)).unwrap();
    let out = p.call(&scope(), &req()).unwrap();
    assert_eq!(out.data, json!({"records": [{"a": 1}]}));
    let raw = h.join().unwrap();
    assert!(raw
        .to_ascii_lowercase()
        .contains("authorization: bearer s3cret"));
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["tenant"], "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    assert_eq!(body["connector"], "ai");
    assert_eq!(body["operation"], "records.extract");
    assert_eq!(body["access"], "read");
}

#[test]
fn a_503_is_unavailable() {
    let (url, _h) = serve_once("503 Service Unavailable", "{}");
    let p = HttpProvider::new(&url, "t", Duration::from_secs(5)).unwrap();
    assert_eq!(
        p.call(&scope(), &req()).unwrap_err(),
        ProviderError::Unavailable
    );
}

#[test]
fn a_connection_refused_is_unavailable() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
    drop(l);
    let p = HttpProvider::new(&url, "t", Duration::from_secs(2)).unwrap();
    assert_eq!(
        p.call(&scope(), &req()).unwrap_err(),
        ProviderError::Unavailable
    );
}

#[test]
fn gateway_error_text_never_comes_back() {
    let (url, _h) = serve_once(
        "500 Internal Server Error",
        r#"{"error":"key sk-123 rejected"}"#,
    );
    let p = HttpProvider::new(&url, "t", Duration::from_secs(5)).unwrap();
    match p.call(&scope(), &req()).unwrap_err() {
        ProviderError::Failed(m) => assert!(!m.contains("sk-123")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_answer_without_data_fails() {
    let (url, _h) = serve_once("200 OK", r#"{"records":[]}"#);
    let p = HttpProvider::new(&url, "t", Duration::from_secs(5)).unwrap();
    assert!(matches!(
        p.call(&scope(), &req()),
        Err(ProviderError::Failed(_))
    ));
}

#[test]
fn redirects_are_not_followed() {
    let (url, _h) = serve_once("302 Found", "{}");
    let p = HttpProvider::new(&url, "t", Duration::from_secs(5)).unwrap();
    assert!(matches!(
        p.call(&scope(), &req()),
        Err(ProviderError::Failed(_))
    ));
}

#[test]
fn urls_must_be_https_or_loopback() {
    let t = Duration::from_secs(1);
    assert!(HttpProvider::new("https://gw.example.com/p", "t", t).is_ok());
    assert!(HttpProvider::new("http://localhost:9/p", "t", t).is_ok());
    assert!(HttpProvider::new("http://[::1]:9/p", "t", t).is_ok());
    assert!(HttpProvider::new("http://gw.example.com/p", "t", t).is_err());
    assert!(HttpProvider::new("https://user:pw@gw.example.com/p", "t", t).is_err());
    assert!(HttpProvider::new("gw.example.com", "t", t).is_err());
    assert!(HttpProvider::new("https://gw.example.com/p", "", t).is_err());
}
