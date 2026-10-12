//! `ce-codegen <manifest.json> [--lang python] [--out FILE]`: writes a typed client.

use std::process::ExitCode;

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut manifest = None;
    let mut out = None;
    let mut lang = "python".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lang" => lang = args.next().ok_or("--lang needs a value")?,
            "--out" => out = Some(args.next().ok_or("--out needs a path")?),
            _ if manifest.is_none() => manifest = Some(a),
            _ => return Err(format!("unexpected argument `{a}`")),
        }
    }
    let path = manifest.ok_or("usage: ce-codegen <manifest.json> [--lang python] [--out FILE]")?;
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
    let m = ce_manifest::validate(&bytes).map_err(|issues| {
        let mut msg = format!("{path} is not a valid manifest:");
        for i in issues {
            msg.push_str(&format!("\n  {} {}: {}", i.path, i.code, i.message));
        }
        msg
    })?;
    let code = match lang.as_str() {
        "python" => ce_codegen::python(&m),
        other => return Err(format!("unknown language `{other}` (use python)")),
    };
    match out {
        Some(p) => std::fs::write(&p, code).map_err(|e| format!("cannot write {p}: {e}")),
        None => {
            print!("{code}");
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("ce-codegen: {msg}");
            ExitCode::FAILURE
        }
    }
}
