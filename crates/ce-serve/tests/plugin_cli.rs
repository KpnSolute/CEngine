//! The owner's plugin commands and the start-up checks, through the real binary.
use serde_json::json;
use std::path::Path;
use std::process::{Command, Output};

const PLUGIN: &str = r#"schema_version = 1
[plugin]
id = "kitchen-rules"
version = "0.1.0"
[compatibility]
engine = ">=0.1.0"
module = "business.inventory"
module_api = 1
[execution]
kind = "policy-pack"
[[policy]]
slot = "over_pull"
rule = "true"
"#;

fn setup(dir: &Path, plugin: &str) {
    let p = dir.join(".cengine/plugins/kitchen-rules");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("plugin.toml"), plugin).unwrap();
    let template = json!({
        "manifest_version": 1,
        "template": {"name": "kitchen", "version": "0.1.0"},
        "entities": {"period": {"fields": {
            "label": {"type": "string", "required": true, "unique": true, "max_length": 80, "indexed": true},
            "status": {"type": "enum", "values": ["open", "closed"], "default": "open", "indexed": true}
        }, "sort_keys": ["label"]}},
        "roles": {"owner": {"read": ["*"], "write": ["*"], "actions": ["*"]}}
    });
    std::fs::write(dir.join("template.json"), template.to_string()).unwrap();
}

fn ce(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ce-serve"));
    c.args(args)
        .env_clear()
        .env("CE_PLUGIN_ROOT", dir)
        .env("CE_INSTALLS", dir.join("installs.json"));
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn digest(dir: &Path) -> String {
    out(&ce(dir, &["plugin", "digest", "kitchen-rules"], &[]))
        .trim()
        .to_string()
}

fn serve_env<'a>(dir: &'a Path, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
    let mut v = vec![
        (
            "CE_DATABASE_URL",
            "postgresql://nobody@127.0.0.1:1/none".to_string(),
        ),
        ("CE_JWT_KEY", "0123456789abcdef0123456789abcdef".to_string()),
        ("CE_INSTALLATION", "kitchen".to_string()),
        (
            "CE_TEMPLATE",
            dir.join("template.json").display().to_string(),
        ),
        ("CE_MODULE", "business.inventory".to_string()),
        ("CE_PLUGIN", "kitchen-rules".to_string()),
    ];
    v.extend(extra.iter().map(|(k, x)| (*k, x.to_string())));
    v
}

fn serve(dir: &Path, args: &[&str]) -> Output {
    let env = serve_env(dir, &[]);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    ce(dir, args, &env)
}

#[test]
fn approving_needs_the_reviewed_digest_and_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    let wrong = format!("sha256:{}", "0".repeat(64));
    let o = ce(
        dir.path(),
        &["plugin", "approve", "kitchen-rules", &wrong],
        &[],
    );
    assert!(!o.status.success());
    assert!(err(&o).contains("nothing approved"), "{}", err(&o));
    assert!(!dir.path().join("installs.json").exists());

    let d = digest(dir.path());
    let o = ce(dir.path(), &["plugin", "approve", "kitchen-rules", &d], &[]);
    assert!(o.status.success(), "{}", err(&o));
    let s = out(&ce(dir.path(), &["plugin", "status"], &[]));
    assert!(
        s.contains("kitchen-rules 0.1.0") && s.contains(&d) && s.contains("active"),
        "{s}"
    );

    ce(dir.path(), &["plugin", "disable", "kitchen-rules"], &[]);
    assert!(out(&ce(dir.path(), &["plugin", "status"], &[])).contains("disabled"));
    // No leftover temp file.
    assert!(!dir.path().join("installs.json.tmp").exists());
}

#[test]
fn a_hand_edited_install_record_stops_everything() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    std::fs::write(
        dir.path().join("installs.json"),
        r#"{"version":1,"plugins":{},"x":1}"#,
    )
    .unwrap();
    let o = ce(dir.path(), &["plugin", "status"], &[]);
    assert!(!o.status.success());
    assert!(
        err(&o).contains("not a valid install record"),
        "{}",
        err(&o)
    );
}

#[test]
fn the_server_refuses_to_start_on_an_unapproved_plugin() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    let o = serve(dir.path(), &["serve"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("not approved"), "{}", err(&o));
}

#[test]
fn the_server_refuses_to_start_when_the_file_changed_after_approval() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    let d = digest(dir.path());
    assert!(
        ce(dir.path(), &["plugin", "approve", "kitchen-rules", &d], &[])
            .status
            .success()
    );
    setup(
        dir.path(),
        &PLUGIN.replace("rule = \"true\"", "rule = \"false\""),
    );
    let o = serve(dir.path(), &["serve"]);
    assert!(!o.status.success());
    assert!(err(&o).contains("DigestMismatch"), "{}", err(&o));
}

#[test]
fn a_manifest_and_a_template_together_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    let env = serve_env(dir.path(), &[("CE_MANIFEST", "/tmp/whatever.json")]);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let o = ce(dir.path(), &["serve"], &env);
    assert!(!o.status.success());
    assert!(err(&o).contains("not both"), "{}", err(&o));
}

/// Installing an approved plugin builds the schema from the composed manifest: the module's
/// tables exist, and the approved plugin's digest is what composed them.
#[test]
fn install_builds_the_schema_of_the_approved_plugin() {
    let Ok(url) = std::env::var("CE_TEST_DATABASE_URL") else {
        eprintln!("skipping: CE_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), PLUGIN);
    let d = digest(dir.path());
    assert!(
        ce(dir.path(), &["plugin", "approve", "kitchen-rules", &d], &[])
            .status
            .success()
    );
    let schema = format!("ce_cli_{}", std::process::id());
    let env = serve_env(
        dir.path(),
        &[
            ("CE_DATABASE_URL", url.as_str()),
            ("CE_SCHEMA", schema.as_str()),
        ],
    );
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let o = ce(dir.path(), &["install"], &env);
    let tables = {
        let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let rows = client
            .query(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = $1",
                &[&schema],
            )
            .unwrap();
        let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
            .unwrap();
        names
    };
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        tables.iter().any(|t| t.contains("stock_line")),
        "{tables:?}"
    );
}
