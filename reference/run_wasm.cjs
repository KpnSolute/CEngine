// Runs the shared expression vectors through the built WebAssembly module.
// Usage: node reference/run_wasm.cjs <dir from `wasm-bindgen --target nodejs --out-dir <dir> ce_wasm.wasm`>
const fs = require("fs");
const path = require("path");
const assert = require("assert");

const m = require(path.resolve(process.argv[2], "ce_wasm.js"));
const root = path.join(__dirname, "..");
const vectors = JSON.parse(fs.readFileSync(path.join(root, "conformance", "expression-vectors.json"), "utf8")).vectors;

let bad = 0;
for (const t of vectors) {
  const got = JSON.parse(m.evalExpr(t.expr, JSON.stringify(t.env)));
  const want = typeof t.out === "boolean" ? String(t.out) : t.out;
  if (!got.ok || got.value !== want) {
    bad++;
    console.log("FAIL", t.expr, want, got);
  }
}

const manifest = fs.readFileSync(path.join(root, "spec", "examples", "inventory.json"), "utf8");
const ok = JSON.parse(m.validateManifest(manifest));
assert(ok.ok && ok.hash.length === 64, JSON.stringify(ok));
assert(!JSON.parse(m.validateManifest("{}")).ok);
const r = JSON.parse(m.validateRecord(manifest, "item", '{"name":"Flour","value":"1"}', "create"));
const codes = new Set(r.errors.map((e) => e.code));
assert(!r.ok && codes.has("computed_field") && codes.has("required"), JSON.stringify(r));
assert(JSON.parse(m.validateRecord(manifest, "item", '{"name":"Flour","unit_cost":"2.5"}', "create")).ok);

console.log(`wasm binding: ${vectors.length - bad} / ${vectors.length} vectors pass`);
process.exit(bad ? 1 : 0);
