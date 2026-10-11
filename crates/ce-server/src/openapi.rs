//! OpenAPI 3.1 document generated from a validated manifest.

use ce_manifest::Manifest;
use serde_json::{json, Map, Value};

fn field_schema(f: &Value) -> Value {
    let mut s = match f["type"].as_str().unwrap_or("") {
        "string" | "text" => json!({"type": "string"}),
        "integer" => json!({"type": "integer"}),
        "decimal" => json!({"type": "number"}),
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
        let mut post_responses = json!({"201": single});
        let mut get_responses = json!({"200": single, "304": {"description": "not modified"}});
        let mut patch_responses = json!({"200": single});
        let mut delete_responses = json!({"204": {"description": "deleted"}});
        for (r, codes) in [
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
            json!({"post": {
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
