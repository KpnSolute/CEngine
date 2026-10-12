//! OpenAPI 3.1 document generated from a validated manifest.

use ce_manifest::Manifest;
use serde_json::{json, Map, Value};

fn field_schema(f: &Value) -> Value {
    let mut s = match f["type"].as_str().unwrap_or("") {
        "string" | "text" => json!({"type": "string"}),
        "integer" => json!({"type": "integer"}),
        "decimal" => {
            json!({"type": ["number", "string"], "description": "plain decimal digits; no exponent"})
        }
        "boolean" => json!({"type": "boolean"}),
        "date" => json!({"type": "string", "format": "date"}),
        "datetime" => json!({"type": "string", "format": "date-time"}),
        "enum" => json!({"type": "string", "enum": f["values"].clone()}),
        "ref" => json!({"type": "string"}),
        _ => json!({}),
    };
    if let Some(n) = f["max_length"].as_u64() {
        s["maxLength"] = json!(n);
    }
    if f["computed"].is_string() {
        s["readOnly"] = json!(true);
    }
    s
}

fn problem_responses(codes: &[(&str, &str)]) -> Value {
    let mut m = Map::new();
    for (status, desc) in codes {
        m.insert(
            status.to_string(),
            json!({"description": desc, "content": {"application/problem+json": {"schema": {"$ref": "#/components/schemas/Problem"}}}}),
        );
    }
    Value::Object(m)
}

pub fn openapi(m: &Manifest, installation: &str) -> Value {
    let mut schemas = Map::new();
    schemas.insert(
        "Problem".into(),
        json!({
            "type": "object",
            "required": ["type", "title", "status", "code", "detail", "request_id", "retryable"],
            "properties": {
                "type": {"type": "string"}, "title": {"type": "string"},
                "status": {"type": "integer"}, "code": {"type": "string"},
                "detail": {"type": "string"}, "request_id": {"type": "string"},
                "retryable": {"type": "boolean"},
                "errors": {"type": "array", "items": {"type": "object", "properties": {
                    "path": {"type": "string"}, "code": {"type": "string"}}}}
            }
        }),
    );
    let mut paths = Map::new();
    let entities = m.value["entities"].as_object().cloned().unwrap_or_default();
    for (name, ent) in &entities {
        let mut props = Map::new();
        props.insert("id".into(), json!({"type": "string", "readOnly": true}));
        props.insert(
            "version".into(),
            json!({"type": "integer", "readOnly": true}),
        );
        let mut required = Vec::new();
        for (fname, f) in ent["fields"].as_object().into_iter().flatten() {
            props.insert(fname.clone(), field_schema(f));
            if f["required"] == true {
                required.push(json!(fname));
            }
        }
        let schema_name = name.clone();
        schemas.insert(
            schema_name.clone(),
            json!({"type": "object", "properties": props, "required": required}),
        );
        let reference = json!({"$ref": format!("#/components/schemas/{schema_name}")});
        let single = json!({"description": "the record", "headers": {"ETag": {"schema": {"type": "string"}}},
            "content": {"application/json": {"schema": {"type": "object", "properties": {
                "data": reference, "meta": {"type": "object"}}}}}});
        let base = format!("/v1/{installation}/{name}");
        let if_match = json!({"name": "If-Match", "in": "header", "required": true, "schema": {"type": "string"}});
        let id_param =
            json!({"name": "id", "in": "path", "required": true, "schema": {"type": "string"}});
        let mut list_params: Vec<Value> = ["select", "sort", "limit", "after", "filter"]
            .iter()
            .map(|p| json!({"name": p, "in": "query", "required": false, "schema": {"type": "string"}}))
            .collect();
        let searchable = ent["fields"]
            .as_object()
            .is_some_and(|fs| fs.values().any(|f| f["searchable"] == true));
        if searchable {
            list_params.push(json!({"name": "q", "in": "query", "required": false,
                "description": "text search over the searchable fields",
                "schema": {"type": "string", "minLength": 1, "maxLength": 100}}));
        }
        let mut list_responses = json!({"200": {"description": "one page",
            "content": {"application/json": {"schema": {"type": "object", "properties": {
                "data": {"type": "array", "items": reference},
                "meta": {"type": "object", "properties": {"next": {"type": ["string", "null"]}}}}}}}}});
        let mut post_responses = json!({"201": single});
        let mut get_responses = json!({"200": single, "304": {"description": "not modified"}});
        let mut patch_responses = json!({"200": single});
        let mut delete_responses = json!({"204": {"description": "deleted"}});
        for (r, codes) in [
            (
                &mut list_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("422", "validation_failed"),
                ][..],
            ),
            (
                &mut post_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("409", "idempotency_conflict"),
                    ("422", "validation_failed"),
                    ("428", "precondition_required"),
                ][..],
            ),
            (
                &mut get_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                ][..],
            ),
            (
                &mut patch_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("409", "version_conflict"),
                    ("422", "validation_failed"),
                    ("428", "precondition_required"),
                ][..],
            ),
            (
                &mut delete_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("409", "version_conflict"),
                    ("428", "precondition_required"),
                ][..],
            ),
        ] {
            if let (Value::Object(r), Value::Object(e)) = (r, problem_responses(codes)) {
                r.extend(e);
            }
        }
        paths.insert(
            base.clone(),
            json!({"get": {
                "operationId": format!("list_{name}"),
                "parameters": list_params,
                "responses": list_responses
            }, "post": {
                "operationId": format!("create_{name}"),
                "parameters": [{"name": "Idempotency-Key", "in": "header", "required": true, "schema": {"type": "string"}}],
                "requestBody": {"required": true, "content": {"application/json": {"schema": reference}}},
                "responses": post_responses
            }}),
        );
        paths.insert(
            format!("{base}/{{id}}"),
            json!({
                "get": {"operationId": format!("get_{name}"), "parameters": [id_param], "responses": get_responses},
                "patch": {"operationId": format!("update_{name}"), "parameters": [id_param, if_match],
                    "requestBody": {"required": true, "content": {"application/merge-patch+json": {"schema": {"type": "object"}}}},
                    "responses": patch_responses},
                "delete": {"operationId": format!("delete_{name}"), "parameters": [id_param, if_match],
                    "responses": delete_responses}
            }),
        );
        // CSV out and in for the entity (see ce-io).
        let mut export_responses = json!({"200": {"description": "the records as CSV",
            "content": {"text/csv": {"schema": {"type": "string"}}}}});
        let mut import_responses = json!({"201": {"description": "rows created"},
            "200": {"description": "dry run: the per-row report"}});
        for (r, codes) in [
            (
                &mut export_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("422", "validation_failed"),
                ][..],
            ),
            (
                &mut import_responses,
                &[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("409", "constraint_violation"),
                    ("413", "payload_too_large"),
                    ("422", "validation_failed"),
                    ("428", "precondition_required"),
                ][..],
            ),
        ] {
            if let (Value::Object(r), Value::Object(e)) = (r, problem_responses(codes)) {
                r.extend(e);
            }
        }
        paths.insert(
            format!("{base}/_export"),
            json!({"get": {"operationId": format!("export_{name}"),
                "parameters": list_params.iter().filter(|p| matches!(p["name"].as_str(), Some("sort" | "filter" | "q"))).cloned().collect::<Vec<_>>(),
                "responses": export_responses}}),
        );
        paths.insert(
            format!("{base}/_import"),
            json!({"post": {"operationId": format!("import_{name}"),
                "parameters": [
                    {"name": "Idempotency-Key", "in": "header", "required": true, "schema": {"type": "string"}},
                    {"name": "dry_run", "in": "query", "required": false, "schema": {"type": "boolean"}}],
                "requestBody": {"required": true, "content": {"text/csv": {"schema": {"type": "string"}}}},
                "responses": import_responses}}),
        );
        // One upload/download path per `file` field; the bytes are the body.
        for (fname, f) in ent["fields"].as_object().into_iter().flatten() {
            if f["type"] != "file" {
                continue;
            }
            let errs = |codes: &[(&str, &str)]| problem_responses(codes);
            let mut put_responses = json!({"200": single});
            if let (Value::Object(r), Value::Object(e)) = (
                &mut put_responses,
                errs(&[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("409", "version_conflict"),
                    ("413", "payload_too_large"),
                    ("422", "validation_failed"),
                    ("428", "precondition_required"),
                    ("503", "files_not_configured"),
                ]),
            ) {
                r.extend(e);
            }
            let mut get_file = json!({"200": {"description": "the file's bytes",
                "headers": {"ETag": {"schema": {"type": "string"}}},
                "content": {"*/*": {"schema": {"type": "string", "format": "binary"}}}}});
            if let (Value::Object(r), Value::Object(e)) = (
                &mut get_file,
                errs(&[
                    ("401", "unauthenticated"),
                    ("403", "forbidden"),
                    ("404", "not_found"),
                    ("503", "files_not_configured"),
                ]),
            ) {
                r.extend(e);
            }
            paths.insert(
                format!("/v1/{installation}/_files/{name}/{{id}}/{fname}"),
                json!({
                    "get": {"operationId": format!("download_{name}_{fname}"),
                        "parameters": [id_param], "responses": get_file},
                    "put": {"operationId": format!("upload_{name}_{fname}"),
                        "parameters": [id_param, if_match,
                            {"name": "X-CE-Filename", "in": "header", "required": false, "schema": {"type": "string"}}],
                        "requestBody": {"required": true, "content": {"*/*": {"schema": {"type": "string", "format": "binary"}}}},
                        "responses": put_responses}
                }),
            );
        }
    }
    let mut barcode_responses = json!({"200": {"description": "the symbol",
        "content": {"image/svg+xml": {"schema": {"type": "string"}}}}});
    if let (Value::Object(r), Value::Object(e)) = (
        &mut barcode_responses,
        problem_responses(&[
            ("401", "unauthenticated"),
            ("404", "not_found"),
            ("422", "validation_failed"),
        ]),
    ) {
        r.extend(e);
    }
    let query = |name: &str, required: bool, schema: Value| json!({"name": name, "in": "query", "required": required, "schema": schema});
    paths.insert(
        format!("/v1/{installation}/_barcode"),
        json!({"get": {"operationId": "barcode",
            "parameters": [
                query("type", true, json!({"type": "string", "enum": ["code128", "ean13", "upca", "qr"]})),
                query("data", true, json!({"type": "string"})),
                query("module_size", false, json!({"type": "integer", "minimum": 1, "maximum": 32})),
                query("bar_height", false, json!({"type": "integer", "minimum": 1, "maximum": 400})),
                query("quiet_zone", false, json!({"type": "integer", "minimum": 0, "maximum": 20}))],
            "responses": barcode_responses}}),
    );
    for (rname, r) in m.value["reports"].as_object().into_iter().flatten() {
        let mut responses = json!({"200": {"description": "the report",
            "content": {"application/json": {"schema": {"type": "object"}},
                "text/csv": {"schema": {"type": "string"}},
                "text/html": {"schema": {"type": "string"}}}}});
        if let (Value::Object(o), Value::Object(e)) = (
            &mut responses,
            problem_responses(&[
                ("401", "unauthenticated"),
                ("403", "forbidden"),
                ("404", "not_found"),
                ("422", "validation_failed"),
            ]),
        ) {
            o.extend(e);
        }
        let mut params = vec![json!({"name": "format", "in": "query", "required": false,
            "schema": {"type": "string", "enum": ["json", "csv", "html"]}})];
        for p in r["params"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            params.push(
                json!({"name": p, "in": "query", "required": false, "schema": {"type": "string"}}),
            );
        }
        paths.insert(
            format!("/v1/{installation}/_report/{rname}"),
            json!({"get": {"operationId": format!("report_{rname}"),
                "summary": r["title"], "parameters": params, "responses": responses}}),
        );
    }
    if crate::slots::enabled(m) {
        let mut responses = json!({"200": {"description": "free slots in time order, at most 1000",
            "content": {"application/json": {"schema": {"type": "object", "properties": {
                "data": {"type": "array", "items": {"type": "object", "properties": {
                    "starts_at": {"type": "string", "format": "date-time"},
                    "ends_at": {"type": "string", "format": "date-time"}}}}}}}}}});
        if let (Value::Object(r), Value::Object(e)) = (
            &mut responses,
            problem_responses(&[
                ("401", "unauthenticated"),
                ("403", "forbidden"),
                ("404", "not_found"),
                ("422", "validation_failed"),
            ]),
        ) {
            r.extend(e);
        }
        paths.insert(
            format!("/v1/{installation}/_slots"),
            json!({"get": {"operationId": "freeSlots",
                "parameters": [
                    query("resource", true, json!({"type": "string"})),
                    query("from", true, json!({"type": "string", "format": "date-time"})),
                    query("to", true, json!({"type": "string", "format": "date-time"})),
                    query("duration", true, json!({"type": "integer", "minimum": 1, "maximum": 1440})),
                    query("step", false, json!({"type": "integer", "minimum": 1, "maximum": 1440}))],
                "responses": responses}}),
        );
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": m.value["template"]["name"].as_str().unwrap_or("CEngine installation"),
            "version": m.hash.clone(),
            "description": "Generated from the template manifest. Draft v1."
        },
        "security": [{"bearer": []}],
        "paths": paths,
        "components": {
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer", "bearerFormat": "JWT"}},
            "schemas": schemas
        }
    })
}
