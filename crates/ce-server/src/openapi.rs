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
        // The create body: the same fields, but `id` is an optional writable UUID (a chosen id)
        // and there is no `version`.
        let create_props = props_for_create(&schemas[&schema_name]);
        let create_name = format!("{schema_name}Create");
        schemas.insert(
            create_name.clone(),
            json!({"type": "object", "properties": create_props, "required": schemas[&schema_name]["required"]}),
        );
        let create_ref = json!({"$ref": format!("#/components/schemas/{create_name}")});
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
                "requestBody": {"required": true, "content": {"application/json": {"schema": create_ref}}},
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
    let mut token_responses = json!({"200": {"description": "a signed token for the installation's tenant",
        "content": {"application/json": {"schema": {"type": "object", "properties": {"data": {"type": "object",
            "properties": {"token": {"type": "string"}, "expires_at": {"type": "integer"},
                "roles": {"type": "array", "items": {"type": "string"}}, "sub": {"type": "string"}}}}}}}}});
    if let (Value::Object(r), Value::Object(e)) = (
        &mut token_responses,
        problem_responses(&[
            ("401", "invalid_credentials"),
            ("404", "not_found"),
            ("422", "validation_failed"),
            ("429", "throttled"),
            ("503", "unavailable"),
        ]),
    ) {
        r.extend(e);
    }
    paths.insert(
        format!("/v1/{installation}/_auth/token"),
        json!({"post": {"operationId": "issueToken", "security": [],
            "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object"}}}},
            "responses": token_responses}}),
    );
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
    let mut ai_responses = json!({"200": {"description": "checked draft records; nothing is saved",
        "content": {"application/json": {"schema": {"type": "object", "properties": {"data": {"type": "object",
            "properties": {"ready": {"type": "integer"},
                "drafts": {"type": "array", "items": {"type": "object", "properties": {
                    "body": {"type": "object"}, "errors": {"type": "array", "items": {"type": "object"}}}}}}}}}}}}});
    if let (Value::Object(r), Value::Object(e)) = (
        &mut ai_responses,
        problem_responses(&[
            ("401", "unauthenticated"),
            ("403", "forbidden"),
            ("422", "validation_failed"),
            ("502", "provider_failed"),
            ("503", "unavailable"),
        ]),
    ) {
        r.extend(e);
    }
    paths.insert(
        format!("/v1/{installation}/_ai/entry"),
        json!({"post": {"operationId": "aiEntry",
            "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object",
                "required": ["entity", "text"],
                "properties": {"entity": {"type": "string"}, "text": {"type": "string", "maxLength": 20000},
                    "hint": {"type": "string", "maxLength": 500}}}}}},
            "responses": ai_responses}}),
    );
    add_operations(m, installation, &mut paths);
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

/// The properties of a record schema as a create body takes them.
fn props_for_create(record: &Value) -> Option<Value> {
    let mut props = record["properties"].as_object()?.clone();
    props.remove("version");
    props.insert(
        "id".into(),
        json!({"type": "string", "format": "uuid",
            "description": "optional: the id to give the new record (unique per tenant and entity)"}),
    );
    Some(Value::Object(props))
}

/// A response map with the listed problem answers added to `ok`.
fn answers(ok: Value, problems: &[(&str, &str)]) -> Value {
    let mut r = ok;
    if let (Value::Object(r), Value::Object(e)) = (&mut r, problem_responses(problems)) {
        r.extend(e);
    }
    r
}

/// The routes that are not plain entity CRUD: batch, actions, period close and reopen, changeset
/// decisions, and event relay and replay. (CSV export and import are documented with each entity.)
fn add_operations(m: &Manifest, installation: &str, paths: &mut Map<String, Value>) {
    let key = json!({"name": "Idempotency-Key", "in": "header", "required": true, "schema": {"type": "string"}});
    let id = json!({"name": "id", "in": "path", "required": true, "schema": {"type": "string"}});
    let base = format!("/v1/{installation}");
    let common = [
        ("401", "unauthenticated"),
        ("403", "forbidden"),
        ("404", "not_found"),
    ];
    let with = |extra: &[(&'static str, &'static str)]| -> Vec<(&'static str, &'static str)> {
        let mut v = common.to_vec();
        v.extend_from_slice(extra);
        v
    };

    // _batch
    paths.insert(
        format!("{base}/_batch"),
        json!({"post": {"operationId": "batch",
            "summary": "up to 100 creates, updates and deletes in one transaction; all apply or none",
            "parameters": [key.clone()],
            "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object",
                "required": ["ops"],
                "properties": {"ops": {"type": "array", "minItems": 1, "maxItems": 100, "items": {
                    "type": "object", "required": ["op", "entity"],
                    "properties": {
                        "op": {"type": "string", "enum": ["create", "update", "delete"]},
                        "entity": {"type": "string"},
                        "id": {"type": "string", "description": "update and delete"},
                        "version": {"type": "integer", "description": "update and delete: the version being changed"},
                        "data": {"type": "object", "description": "create and update"}}}}}}}}},
            "responses": answers(
                json!({"200": {"description": "one entry per operation, in order (null for a delete)",
                    "content": {"application/json": {"schema": {"type": "object", "properties": {
                        "data": {"type": "array", "items": {"type": ["object", "null"]}},
                        "meta": {"type": "object"}}}}}}}),
                &with(&[("409", "conflict"), ("422", "validation_failed")]))}}),
    );

    // actions
    for (name, a) in m.value["actions"].as_object().into_iter().flatten() {
        let entity = a["entity"].as_str().unwrap_or_default();
        let record = if m.value["entities"][entity].is_object() {
            json!({"$ref": format!("#/components/schemas/{entity}")})
        } else {
            json!({"type": "object"})
        };
        paths.insert(
            format!("{base}/actions/{name}"),
            json!({"post": {"operationId": format!("action_{name}"),
                "summary": format!("run `{name}` on one {entity} record"),
                "parameters": [key.clone(),
                    {"name": "If-Match", "in": "header", "required": false, "schema": {"type": "string"},
                        "description": "the record version the caller read"}],
                "requestBody": {"required": true, "content": {"application/json": {"schema": {"type": "object",
                    "required": ["id"],
                    "properties": {"id": {"type": "string", "description": "the record the action runs on"},
                        "input": {"type": "object", "description": "arguments the action reads"}}}}}},
                "responses": answers(
                    json!({"200": {"description": "the record after the action",
                        "content": {"application/json": {"schema": {"type": "object", "properties": {
                            "data": record,
                            "meta": {"type": "object", "properties": {
                                "request_id": {"type": "string"}, "version": {"type": "integer"},
                                "created": {"type": "array", "items": {"type": "object"}},
                                "emitted": {"type": "array", "items": {"type": "object"}}}}}}}}}}),
                    &with(&[("409", "invalid_transition or version_conflict"), ("422", "validation_failed")]))}}),
        );
    }

    // period close and reopen
    if crate::periods::enabled(m) {
        for verb in ["close", "reopen"] {
            paths.insert(
                format!("{base}/period/{{id}}/{verb}"),
                json!({"post": {"operationId": format!("period_{verb}"),
                    "summary": format!("{verb} a period"),
                    "parameters": [id.clone()],
                    "responses": answers(
                        json!({"200": {"description": "the period after the change",
                            "content": {"application/json": {"schema": {"type": "object", "properties": {
                                "data": {"$ref": "#/components/schemas/period"}, "meta": {"type": "object"}}}}}}}),
                        &with(&[("409", "invalid_transition")]))}}),
            );
        }
    }

    // changeset decisions
    if crate::changesets::enabled(m) {
        for (verb, what) in [
            ("merge", "apply the changeset's operations"),
            ("reject", "close the changeset without applying it"),
            ("revert", "undo a merged changeset"),
            ("withdraw", "the submitter withdraws their own changeset"),
            ("preview", "show what a merge would store, and keep nothing"),
        ] {
            paths.insert(
                format!("{base}/changeset/{{id}}/{verb}"),
                json!({"post": {"operationId": format!("changeset_{verb}"), "summary": what,
                    "parameters": [id.clone()],
                    "responses": answers(
                        json!({"200": {"description": if verb == "preview" { "what a merge would store" } else { "the changeset after the decision" },
                            "content": {"application/json": {"schema": {"type": "object", "properties": {
                                "data": {"type": "object"}, "meta": {"type": "object"}}}}}}}),
                        &with(&[("409", "invalid_transition"), ("422", "validation_failed")]))}}),
            );
        }
    }

    // outbound events
    if crate::events::enabled(m) {
        paths.insert(
            format!("{base}/events/relay"),
            json!({"post": {"operationId": "eventsRelay", "summary": "one delivery pass for the tenant",
                "responses": answers(
                    json!({"200": {"description": "counts for the pass",
                        "content": {"application/json": {"schema": {"type": "object", "properties": {"data": {"type": "object",
                            "properties": {"delivered": {"type": "integer"}, "retrying": {"type": "integer"},
                                "dead_lettered": {"type": "integer"}}}}}}}}}),
                    &with(&[("503", "unavailable")]))}}),
        );
        paths.insert(
            format!("{base}/events/{{id}}/replay"),
            json!({"post": {"operationId": "eventsReplay", "summary": "queue a delivered or dead-lettered event again",
                "parameters": [id.clone()],
                "responses": answers(
                    json!({"200": {"description": "the event, queued again",
                        "content": {"application/json": {"schema": {"type": "object"}}}}}),
                    &with(&[("409", "invalid_transition")]))}}),
        );
    }
}
