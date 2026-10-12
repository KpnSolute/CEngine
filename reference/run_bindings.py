"""Runs the shared expression vectors through the built `cengine` Python extension.

Usage: python reference/run_bindings.py <path to libcengine.so | cengine.so | cengine.pyd>
"""
import importlib.util
import json
import os
import shutil
import sys
import tempfile

src = sys.argv[1]
tmp = tempfile.mkdtemp()
ext = ".pyd" if sys.platform == "win32" else ".so"
shutil.copy(src, os.path.join(tmp, "cengine" + ext))
sys.path.insert(0, tmp)
import cengine  # noqa: E402

root = os.path.join(os.path.dirname(__file__), "..")
vectors = json.load(open(os.path.join(root, "conformance", "expression-vectors.json")))["vectors"]
bad = 0
for t in vectors:
    got = json.loads(cengine.eval_expr(t["expr"], json.dumps(t["env"])))
    want = t["out"]
    if isinstance(want, bool):
        want = "true" if want else "false"
    if not got["ok"] or got["value"] != want:
        bad += 1
        print("FAIL", t["expr"], want, got)

manifest = open(os.path.join(root, "spec", "examples", "inventory.json")).read()
m = json.loads(cengine.validate_manifest(manifest))
assert m["ok"] and len(m["hash"]) == 64, m
assert not json.loads(cengine.validate_manifest("{}"))["ok"]
r = json.loads(cengine.validate_record(manifest, "item", '{"name": "Flour", "value": "1"}', "create"))
assert not r["ok"] and {"computed_field", "required"} <= {e["code"] for e in r["errors"]}, r
assert json.loads(cengine.validate_record(manifest, "item", '{"name": "Flour", "unit_cost": "2.5"}', "create"))["ok"]

print("python binding:", len(vectors) - bad, "/", len(vectors), "vectors pass")
sys.exit(1 if bad else 0)
