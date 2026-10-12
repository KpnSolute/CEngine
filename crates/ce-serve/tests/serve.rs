//! Runs the real binary: `install`, then `serve`, then asks it for its health.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn database_url() -> Option<String> {
    match std::env::var("CE_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ if std::env::var("CE_REQUIRE_DB").is_ok() => {
            panic!("CE_REQUIRE_DB is set but CE_TEST_DATABASE_URL is not")
        }
        _ => None,
    }
}

fn command(url: &str, schema: &str, bind: &str, arg: &str) -> Command {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spec/examples/inventory.json"
    );
    let mut c = Command::new(env!("CARGO_BIN_EXE_ce-serve"));
    c.arg(arg)
        .env("CE_DATABASE_URL", url)
        .env("CE_MANIFEST", manifest)
        .env("CE_JWT_KEY", "0123456789abcdef0123456789abcdef")
        .env("CE_INSTALLATION", "test-inst")
        .env("CE_SCHEMA", schema)
        .env("CE_BIND", bind)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    c
}

fn get(bind: &str, path: &str) -> Option<String> {
    let mut s = TcpStream::connect(bind).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut out = String::new();
    s.read_to_string(&mut out).ok()?;
    Some(out)
}

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn installs_then_serves() {
    let Some(url) = database_url() else { return };
    let schema = format!("ce_serve_test_{}", std::process::id());
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bind = format!("127.0.0.1:{port}");

    // Serving before installing fails with a hint.
    let out = command(&url, &schema, &bind, "serve").output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ce-serve install"));

    assert!(command(&url, &schema, &bind, "install")
        .status()
        .unwrap()
        .success());
    // A second install refuses to touch the existing schema.
    assert!(!command(&url, &schema, &bind, "install")
        .status()
        .unwrap()
        .success());

    let server = Kill(command(&url, &schema, &bind, "serve").spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let resp = loop {
        if let Some(r) = get(&bind, "/_meta/health/ready") {
            break r;
        }
        assert!(Instant::now() < deadline, "server did not come up");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    // Data routes need a token.
    let denied = get(&bind, "/v1/test-inst/item").unwrap();
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");
    drop(server);

    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .unwrap();
}

/// One HTTP request with a body; returns (status line, body bytes).
fn request(
    bind: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (String, Vec<u8>) {
    let mut s = TcpStream::connect(bind).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let i = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = String::from_utf8_lossy(&raw[..i])
        .lines()
        .next()
        .unwrap()
        .to_string();
    (status, raw[i + 4..].to_vec())
}

/// A gateway that answers every call with one canned AI extraction.
fn gateway() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/provider",
        l.local_addr().unwrap().port()
    );
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { return };
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = c.read(&mut buf).unwrap_or(0);
                got.extend_from_slice(&buf[..n]);
                let t = String::from_utf8_lossy(&got).to_string();
                if n == 0 {
                    break;
                }
                if let Some(i) = t.find("\r\n\r\n") {
                    let len = t[..i]
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
            let body = r#"{"data":{"records":[{"title":"Receipt 17"}]}}"#;
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

#[test]
fn files_and_ai_entry_are_switched_on_by_environment() {
    let Some(url) = database_url() else { return };
    let schema = format!("ce_serve_files_{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let manifest = serde_json::json!({
        "manifest_version": 1,
        "template": {"name": "docs", "version": "0.1.0"},
        "entities": {"doc": {"fields": {
            "title": {"type": "string", "max_length": 80, "required": true, "indexed": true},
            "scan": {"type": "file"}
        }, "sort_keys": ["title"]}},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    std::fs::write(dir.path().join("m.json"), manifest.to_string()).unwrap();
    let hash = ce_server::hash_pin_random("2468").unwrap();
    std::fs::write(
        dir.path().join("staff.json"),
        serde_json::json!({"users": [{"username": "ann", "pin_hash": hash, "roles": ["owner"]}]})
            .to_string(),
    )
    .unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bind = format!("127.0.0.1:{port}");
    let gw = gateway();
    let cmd = |arg: &str, on: bool| {
        let mut c = command(&url, &schema, &bind, arg);
        c.env("CE_MANIFEST", dir.path().join("m.json"))
            .env("CE_STAFF_FILE", dir.path().join("staff.json"))
            .env("CE_TENANT", "00000000-0000-4000-8000-000000000001")
            .env("CE_ROLE_LADDER", "owner");
        if on {
            c.env("CE_FILES", "postgres")
                .env("CE_PROVIDER_URL", &gw)
                .env("CE_PROVIDER_TOKEN", "gateway-secret");
        }
        c
    };
    assert!(cmd("install", false).status().unwrap().success());

    // Off by default: both routes answer 503.
    {
        let server = Kill(cmd("serve", false).spawn().unwrap());
        wait_ready(&bind);
        let tok = sign_in(&bind);
        let h = [
            ("Authorization", tok.as_str()),
            ("Content-Type", "application/json"),
        ];
        let (st, _) = request(
            &bind,
            "POST",
            "/v1/test-inst/_ai/entry",
            &h,
            br#"{"entity":"doc","text":"x"}"#,
        );
        assert!(st.contains("503"), "{st}");
        drop(server);
    }

    // On: a file round-trips through Postgres and the gateway extracts a draft.
    let server = Kill(cmd("serve", true).spawn().unwrap());
    wait_ready(&bind);
    let tok = sign_in(&bind);
    let json_h = [
        ("Authorization", tok.as_str()),
        ("Content-Type", "application/json"),
        ("Idempotency-Key", "k1"),
    ];
    let (st, body) = request(
        &bind,
        "POST",
        "/v1/test-inst/doc",
        &json_h,
        br#"{"title":"Scan"}"#,
    );
    assert!(st.contains("201"), "{st}");
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = created["data"]["id"].as_str().unwrap().to_string();
    let put_h = [
        ("Authorization", tok.as_str()),
        ("Content-Type", "application/pdf"),
        ("If-Match", "1"),
    ];
    let path = format!("/v1/test-inst/_files/doc/{id}/scan");
    let (st, _) = request(&bind, "PUT", &path, &put_h, b"%PDF-1.4 bytes");
    assert!(st.contains("200"), "{st}");
    let (st, bytes) = request(&bind, "GET", &path, &[("Authorization", tok.as_str())], b"");
    assert!(st.contains("200"), "{st}");
    assert_eq!(bytes, b"%PDF-1.4 bytes");

    let (st, body) = request(
        &bind,
        "POST",
        "/v1/test-inst/_ai/entry",
        &json_h,
        br#"{"entity":"doc","text":"Receipt 17"}"#,
    );
    assert!(
        st.contains("200"),
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["data"]["drafts"][0]["body"]["title"], "Receipt 17");
    assert_eq!(v["data"]["ready"], 1);
    drop(server);

    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let n: i64 = client
        .query_one(
            &format!(r#"SELECT count(*) FROM "{schema}"."_ce_blobs""#),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .unwrap();
}

fn wait_ready(bind: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while get(bind, "/_meta/health/ready").is_none_or(|r| !r.starts_with("HTTP/1.1 200")) {
        assert!(Instant::now() < deadline, "server did not come up");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Signs in as `ann` and returns the `Authorization` header value.
fn sign_in(bind: &str) -> String {
    let (st, body) = request(
        bind,
        "POST",
        "/v1/test-inst/_auth/token",
        &[("Content-Type", "application/json")],
        br#"{"username":"ann","pin":"2468"}"#,
    );
    assert!(
        st.contains("200"),
        "{st} {}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    format!("Bearer {}", v["data"]["token"].as_str().unwrap())
}
