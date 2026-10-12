use std::path::PathBuf;
use std::process::Command;

fn manifest(name: &str) -> ce_manifest::Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    ce_manifest::validate(&std::fs::read(path).unwrap()).expect("example is valid")
}

/// True when `cmd` runs. CI sets CE_REQUIRE_TOOLS, and then a missing tool fails the test
/// instead of skipping it.
fn have(cmd: &str) -> bool {
    let ok = Command::new(cmd)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        ok || std::env::var("CE_REQUIRE_TOOLS").is_err(),
        "{cmd} is required in CI"
    );
    ok
}

/// Node 22.6 or newer can run TypeScript files directly.
fn node_runs_typescript() -> bool {
    if !have("node") {
        return false;
    }
    let out = Command::new("node").arg("--version").output().unwrap();
    let v = String::from_utf8_lossy(&out.stdout);
    let mut parts = v.trim().trim_start_matches('v').split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor) >= (22, 6)
}

fn write(name: &str, code: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ce-codegen-ts-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("client.ts");
    std::fs::write(&path, code).unwrap();
    path
}

#[test]
fn every_example_generates_typescript_that_typechecks() {
    if !have("tsc") {
        return;
    }
    for name in ["inventory", "booking_crm", "approval_workflow"] {
        let path = write(name, &ce_codegen::typescript(&manifest(name)));
        let out = Command::new("tsc")
            .args([
                "--noEmit",
                "--strict",
                "--erasableSyntaxOnly",
                "--target",
                "es2022",
                "--module",
                "esnext",
                "--moduleResolution",
                "bundler",
                "--lib",
                "es2022,dom",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn generated_filters_and_sorts_behave() {
    if !node_runs_typescript() {
        return;
    }
    let path = write("behave", &ce_codegen::typescript(&manifest("inventory")));
    let dir = path.parent().unwrap();
    let script = r#"
import { ItemFields as F, and, or, not, Client } from "./client.ts";

const f = or(and(F.unit_cost.gte("1.5"), not(F.name.eq("Salt"))), F.category.in(["dry", "cold"]));
const want = { or: [
  { and: [{ field: "unit_cost", op: "gte", value: "1.5" }, { not: { field: "name", op: "eq", value: "Salt" } }] },
  { field: "category", op: "in", value: ["dry", "cold"] }] };
if (JSON.stringify(f.node) !== JSON.stringify(want)) throw new Error(f.toJSON());
if (F.period.isNull().toJSON() !== '{"field":"period","op":"null","value":true}') throw new Error("null");
if (String(F.name.desc()) !== "-name" || String(F.name.asc()) !== "name") throw new Error("sort");
let refused = false;
try { F.unit_cost.gt(1.5 as unknown as string); } catch { refused = true; }
if (!refused) throw new Error("fractional number accepted");

// Requests: the right URL, headers and body, against a stand-in fetch.
const seen: any[] = [];
const c = new Client({ baseUrl: "http://h/", installation: "i1", token: "T", fetch: (async (url: string, init: any) => {
  seen.push({ url, init });
  return new Response(JSON.stringify({ data: { id: "a", version: 2, name: "x", unit_cost: "1" }, meta: { next: null } }), { status: 200 });
}) as any });
const rec = await c.item.update({ id: "a", version: 1 }, { name: "x" });
if (rec.version !== 2) throw new Error("update result");
const u = seen[0];
if (u.url !== "http://h/v1/i1/item/a" || u.init.method !== "PATCH" || u.init.headers["If-Match"] !== '"1"'
    || u.init.headers.Authorization !== "Bearer T" || u.init.body !== '{"name":"x"}') throw new Error(JSON.stringify(u));
await c.item.create({ name: "n", unit_cost: "2" });
const k = seen[1].init.headers["Idempotency-Key"];
if (typeof k !== "string" || k.length < 30 || seen[1].init.method !== "POST") throw new Error("create");
await c.item.list({ where: F.name.eq("a"), sort: [F.name.desc()], limit: 5, q: "fl" });
const q = new URL(seen[2].url).searchParams;
if (q.get("filter") !== '{"field":"name","op":"eq","value":"a"}' || q.get("sort") !== "-name"
    || q.get("limit") !== "5" || q.get("q") !== "fl") throw new Error(seen[2].url);
console.log("ok");
"#;
    std::fs::write(dir.join("check.ts"), script).unwrap();
    let out = Command::new("node")
        .args(["--experimental-strip-types", "check.ts"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).trim().ends_with("ok"));
}
