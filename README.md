# CEngine

ComputeEngine v1: a library KpnCompute uses to run any type of template. It is a
separate repository in the KpnSolute organization and is not a product or a
service. Governed by Loom ADR-0007 (KpnDream/loom, approved with conditions;
library build authorized by the owner on 2026-10-10).

Status: pre-release. Nothing here is deployed or used by KpnCompute yet.

## Layout
- `spec/`: manifest JSON Schema, catalogs, example templates, expression language and language plan (drafts).
- `conformance/`: vectors every language binding must pass.
- `reference/`: tiny Python evaluator used only to check the vectors are self-consistent.
- `crates/ce-expr`: Rust expression lexer, parser and evaluator.

## Check
```
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
python3 reference/run_vectors.py
```

## Not done yet
Manifest validation crate, query planner, store, events, server, Python and WASM bindings,
date and aggregate functions, static type checking of expressions.
