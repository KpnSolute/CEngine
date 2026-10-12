use std::path::PathBuf;
use std::process::Command;

fn manifest(name: &str) -> ce_manifest::Manifest {
    let path = format!(
        "{}/../../spec/examples/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    ce_manifest::validate(&std::fs::read(path).unwrap()).expect("example is valid")
}

fn python3() -> Option<&'static str> {
    Command::new("python3")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "python3")
}

fn write(name: &str, code: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ce-codegen-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("client.py");
    std::fs::write(&path, code).unwrap();
    path
}

fn run(py: &str, dir: &std::path::Path, code: &str) -> String {
    let out = Command::new(py)
        .arg("-c")
        .arg(code)
        .current_dir(dir)
        .env("PYTHONPATH", dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "python failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn every_example_generates_python_that_compiles() {
    let Some(py) = python3() else {
        assert!(
            std::env::var("CE_REQUIRE_TOOLS").is_err(),
            "python3 is required in CI"
        );
        return;
    };
    for name in ["inventory", "booking_crm", "approval_workflow"] {
        let code = ce_codegen::python(&manifest(name));
        let path = write(name, &code);
        let out = Command::new(py)
            .args(["-m", "py_compile"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn generated_types_and_filters_behave() {
    let Some(py) = python3() else { return };
    let code = ce_codegen::python(&manifest("inventory"));
    let path = write("behave", &code);
    let dir = path.parent().unwrap();
    let script = r#"
import json
from decimal import Decimal
from datetime import date
import client as c

# Records decode decimals and dates exactly, and writes never include computed fields.
item = c.Item.from_json({"id": "i1", "version": 3, "name": "Flour", "unit_cost": "2.5000",
                         "on_hand": "10.000", "value": "25.00", "category": "dry"})
assert item.unit_cost == Decimal("2.5000") and item.value == Decimal("25.00")
assert item.version == 3 and item.id == "i1"
body = item.to_json()
assert "value" not in body and "id" not in body and body["unit_cost"] == "2.5000", body
assert c.Period.from_json({"id": "p", "version": 1, "month": "2026-10-01"}).month == date(2026, 10, 1)

# Filters build the JSON the server's planner takes.
f = (c.Item.f.unit_cost >= Decimal("1.5")) & ~(c.Item.f.name == "Salt") | c.Item.f.category.is_in(["dry", "cold"])
assert json.loads(f.to_json()) == {"or": [
    {"and": [{"field": "unit_cost", "op": "gte", "value": "1.5"},
             {"not": {"field": "name", "op": "eq", "value": "Salt"}}]},
    {"field": "category", "op": "in", "value": ["dry", "cold"]}]}, f.to_json()
assert json.loads(c.Item.f.period.is_null().to_json()) == {"field": "period", "op": "null", "value": True}
assert str(c.Item.f.name.desc()) == "-name" and str(c.Item.f.name.asc()) == "name"

# Floats are refused for money.
try:
    c.Item.f.unit_cost > 1.5
    raise SystemExit("float accepted")
except TypeError:
    pass
print("ok")
"#;
    assert_eq!(run(py, dir, script).trim(), "ok");
}

#[test]
fn awkward_names_are_made_safe() {
    let Some(py) = python3() else { return };
    let m = ce_manifest::validate(
        br#"{"manifest_version":1,"template":{"name":"odd","version":"0.1.0"},
             "entities":{"class":{"fields":{"id_":{"type":"string"},"from":{"type":"string"},"f":{"type":"integer"}}}}}"#,
    )
    .expect("manifest is valid");
    let code = ce_codegen::python(&m);
    let path = write("odd", &code);
    let dir = path.parent().unwrap();
    let script = r#"
import client as c
r = c.Class.from_json({"id": "x", "version": 1, "from": "a", "f": 4})
assert r.from_ == "a" and r.f_ == 4
assert r.to_json() == {"from": "a", "f": 4}
print("ok")
"#;
    assert_eq!(run(py, dir, script).trim(), "ok");
}
