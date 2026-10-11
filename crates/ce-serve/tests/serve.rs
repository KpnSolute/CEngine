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
