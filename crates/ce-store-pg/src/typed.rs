//! Typed Postgres store (draft v1): the `Store` contract over the per-entity tables that
//! `ce-migrate` generates, so the query planner and the store read and write the same rows.
//!
//! Compared with the generic JSONB store: every field is a real, typed column, so database
//! constraints (not null, unique, enum and length checks, tenant-scoped foreign keys) apply
//! to every write. The store re-validates every body with `ce-validate` and fills in what the
//! engine owns: ids, versions, timestamps, `default` values and the initial state.
//!
//! Reading: absent and null are the same (null columns are omitted). Decimals come back as
//! strings so no digit is lost to floating point; dates and datetimes as ISO strings.
//! Computed columns are read but never written yet (no evaluator); they stay null.
//! On update the state field and computed fields in the incoming data are ignored, so a
//! read-merge-write cycle cannot move state.

use ce_manifest::Manifest;
use ce_query::{plan_list, ListRequest, Param, Target};
use ce_store::{Page, Record, Store, StoreError, TenantScope};
use ce_validate::{validate, Mode};
use postgres::{types::ToSql, Client, NoTls};
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Decimal,
}

struct Col {
    name: String,
    cast: &'static str,
    ty: String,
    kind: Kind,
    default: Option<Value>,
    /// Declared state field or computed: never written from caller data.
    managed: bool,
}

struct State {
    field: String,
    initial: String,
    declared: bool,
}

struct Ent {
    cols: Vec<Col>,
    soft: bool,
    state: Option<State>,
}

pub struct TypedPgStore {
    client: Mutex<Client>,
    schema: String,
    manifest: Manifest,
    ents: HashMap<String, Ent>,
}

fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn db(e: postgres::Error) -> StoreError {
    match e.code().map(|c| c.code()) {
        Some("23505") => StoreError::Constraint("unique"),
        Some("23503") => StoreError::Constraint("reference"),
        Some("23514") => StoreError::Constraint("check"),
        Some("23502") => StoreError::Constraint("required"),
        Some(c) if c.starts_with("22") => {
            StoreError::BadData("a value is malformed or out of range")
        }
        _ => StoreError::Backend(e.to_string()),
    }
}

fn cast_of(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "string" | "text" | "enum" => "text",
        "integer" => "bigint",
        "decimal" => "numeric",
        "boolean" => "boolean",
        "date" => "date",
        "datetime" => "timestamptz",
        "ref" => "uuid",
        "json" | "file" => "jsonb",
        _ => return None,
    })
}

impl Ent {
    /// The columns every read returns, in a fixed order: id, version, then these.
    fn select_list(&self) -> String {
        let mut out = vec!["\"id\"::text".to_string(), "\"version\"".to_string()];
        for c in &self.cols {
            out.push(match c.kind {
                Kind::Decimal => format!("\"{}\"::text", c.name),
                Kind::Plain => format!("to_jsonb(\"{}\")", c.name),
            });
        }
        if let Some(s) = self.state.as_ref().filter(|s| !s.declared) {
            out.push(format!("to_jsonb(\"{}\")", s.field));
        }
        out.join(", ")
    }

    fn record(&self, entity: &str, row: &postgres::Row) -> Record {
        let mut data = Map::new();
        for (i, c) in self.cols.iter().enumerate() {
            let v: Option<Value> = match c.kind {
                Kind::Decimal => row.get::<_, Option<String>>(2 + i).map(Value::String),
                Kind::Plain => row.get::<_, Option<Value>>(2 + i),
            };
            if let Some(v) = v {
                data.insert(c.name.clone(), v);
            }
        }
        if let Some(s) = self.state.as_ref().filter(|s| !s.declared) {
            if let Some(v) = row.get::<_, Option<Value>>(2 + self.cols.len()) {
                data.insert(s.field.clone(), v);
            }
        }
        Record {
            id: row.get(0),
            entity: entity.to_string(),
            version: row.get::<_, i64>(1) as u64,
            data: Value::Object(data),
        }
    }
}

/// How a JSON value is sent: as text, cast to the column type in SQL.
fn param_text(ty: &str, v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => {
            debug_assert!(matches!(ty, "json" | "file"));
            other.to_string()
        }
    }
}

fn refs(params: &[Option<String>]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

impl TypedPgStore {
    /// Creates the schema, tables and idempotency table for a fresh installation, then
    /// returns the store. Fails if the schema already exists.
    pub fn install(url: &str, m: &Manifest, schema: &str) -> Result<Self, StoreError> {
        let ddl = ce_migrate::generate(m, schema)
            .map_err(|_| StoreError::BadData("manifest cannot be installed"))?;
        let mut client = Client::connect(url, NoTls).map_err(db)?;
        let mut tx = client.transaction().map_err(db)?;
        for stmt in &ddl {
            tx.batch_execute(stmt).map_err(db)?;
        }
        tx.batch_execute(&format!(
            r#"CREATE TABLE "{schema}"."_ce_idempotency" (
                   tenant_id uuid  NOT NULL,
                   key       text  NOT NULL,
                   entity    text  NOT NULL,
                   payload   jsonb NOT NULL,
                   result    jsonb,
                   PRIMARY KEY (tenant_id, key)
               )"#
        ))
        .map_err(db)?;
        tx.commit().map_err(db)?;
        Self::build(client, m, schema)
    }

    /// Connects to an installed schema. Does not change the database.
    pub fn open(url: &str, m: &Manifest, schema: &str) -> Result<Self, StoreError> {
        let client = Client::connect(url, NoTls).map_err(db)?;
        Self::build(client, m, schema)
    }

    fn build(client: Client, m: &Manifest, schema: &str) -> Result<Self, StoreError> {
        if !ident_ok(schema) {
            return Err(StoreError::BadData("invalid schema name"));
        }
        let bad = StoreError::BadData("manifest has a field the store cannot map");
        let mut ents = HashMap::new();
        for (name, e) in m.value["entities"].as_object().into_iter().flatten() {
            if !ident_ok(name) {
                return Err(bad);
            }
            let state = e["state"]["field"].as_str().map(|f| State {
                field: f.to_string(),
                initial: e["state"]["initial"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                declared: e["fields"][f].is_object(),
            });
            let mut cols = Vec::new();
            let mut fields: Vec<(&String, &Value)> =
                e["fields"].as_object().into_iter().flatten().collect();
            fields.sort_by_key(|(k, _)| *k);
            for (fname, f) in fields {
                let ty = f["type"].as_str().unwrap_or_default();
                let (true, Some(cast)) = (ident_ok(fname), cast_of(ty)) else {
                    return Err(bad);
                };
                // No evaluator exists yet, so a required computed column could never be filled.
                if f["computed"].is_string() && f["required"] == true {
                    return Err(StoreError::BadData(
                        "a computed field cannot be required until computed fields are evaluated",
                    ));
                }
                cols.push(Col {
                    name: fname.clone(),
                    cast,
                    ty: ty.to_string(),
                    kind: if ty == "decimal" {
                        Kind::Decimal
                    } else {
                        Kind::Plain
                    },
                    default: f.get("default").filter(|d| !d.is_null()).cloned(),
                    managed: f["computed"].is_string()
                        || state.as_ref().is_some_and(|s| s.field == *fname),
                });
            }
            if let Some(s) = &state {
                if !ident_ok(&s.field) {
                    return Err(bad);
                }
            }
            ents.insert(
                name.clone(),
                Ent {
                    cols,
                    soft: e["soft_delete"] != false,
                    state,
                },
            );
        }
        Ok(TypedPgStore {
            client: Mutex::new(client),
            schema: schema.to_string(),
            manifest: m.clone(),
            ents,
        })
    }

    pub fn schema_name(&self) -> &str {
        &self.schema
    }

    /// Drops the schema and everything in it. For tests.
    pub fn drop_schema(&self) -> Result<(), StoreError> {
        self.lock()
            .batch_execute(&format!(
                r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#,
                self.schema
            ))
            .map_err(db)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Client> {
        self.client.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ent(&self, entity: &str) -> Result<&Ent, StoreError> {
        self.ents.get(entity).ok_or(StoreError::NotFound)
    }

    fn tenant_and_id<'a>(scope: &'a TenantScope, id: &'a str) -> Option<(&'a str, &'a str)> {
        (is_uuid(scope.tenant_id()) && is_uuid(id)).then_some((scope.tenant_id(), id))
    }

    fn check_body(
        &self,
        entity: &str,
        data: &Map<String, Value>,
        mode: Mode,
    ) -> Result<(), StoreError> {
        validate(&self.manifest, entity, data, mode)
            .map_err(|_| StoreError::BadData("the record does not match the manifest"))
    }
}

fn replay(result: Option<Value>) -> Result<Record, StoreError> {
    let v = result.ok_or(StoreError::NotFound)?;
    Ok(Record {
        id: v["id"].as_str().ok_or(StoreError::NotFound)?.to_string(),
        entity: v["entity"].as_str().unwrap_or_default().to_string(),
        version: v["version"].as_u64().ok_or(StoreError::NotFound)?,
        data: v["data"].clone(),
    })
}

impl Store for TypedPgStore {
    fn list(
        &self,
        scope: &TenantScope,
        entity: &str,
        req: &ListRequest,
    ) -> Result<Page, StoreError> {
        let ent = self
            .ents
            .get(entity)
            .ok_or(StoreError::BadData("unknown entity"))?;
        let limit = req
            .limit
            .unwrap_or(ce_query::DEFAULT_LIMIT)
            .clamp(1, ce_query::MAX_LIMIT);
        // The cursor is built from the last row's sort values, so sort fields are always read.
        let mut req = req.clone();
        let mut hidden: Vec<String> = Vec::new();
        if let Some(sel) = req.select.as_mut() {
            for k in &req.sort {
                if k.field != "id" && !sel.contains(&k.field) {
                    sel.push(k.field.clone());
                    hidden.push(k.field.clone());
                }
            }
        }
        let target = Target {
            schema: self.schema.clone(),
            entity: entity.to_string(),
        };
        let planned = plan_list(&self.manifest, &target, scope, &req).map_err(StoreError::Query)?;
        let params: Vec<Box<dyn ToSql + Sync>> = planned
            .params
            .iter()
            .map(|p| match p {
                Param::Text(s) | Param::Dec(s) => Box::new(s.clone()) as Box<dyn ToSql + Sync>,
                Param::Int(i) => Box::new(*i),
                Param::Bool(b) => Box::new(*b),
            })
            .collect();
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|b| b.as_ref()).collect();
        let rows = self
            .client
            .lock()
            .map_err(|_| StoreError::Backend("poisoned".into()))?
            .query(planned.sql.as_str(), &refs)
            .map_err(db)?;
        let mut records: Vec<Record> = rows
            .iter()
            .map(|row| {
                let mut data = Map::new();
                for (i, name) in planned.columns.iter().enumerate() {
                    let decimal = ent
                        .cols
                        .iter()
                        .any(|c| &c.name == name && matches!(c.kind, Kind::Decimal));
                    let v = if decimal {
                        row.get::<_, Option<String>>(2 + i).map(Value::String)
                    } else {
                        row.get::<_, Option<Value>>(2 + i)
                    };
                    if let Some(v) = v.filter(|v| !v.is_null()) {
                        data.insert(name.clone(), v);
                    }
                }
                Record {
                    id: row.get(0),
                    entity: entity.to_string(),
                    version: row.get::<_, i64>(1) as u64,
                    data: Value::Object(data),
                }
            })
            .collect();
        // The planner reads one extra row to learn whether another page exists.
        let next = if records.len() > limit {
            records.truncate(limit);
            let last = records.last().expect("limit is at least 1");
            let mut cursor = Vec::new();
            for k in &req.sort {
                cursor.push(if k.field == "id" {
                    Value::String(last.id.clone())
                } else {
                    // Null columns are omitted from the record; the cursor says null.
                    last.data.get(&k.field).cloned().unwrap_or(Value::Null)
                });
            }
            if !req.sort.iter().any(|k| k.field == "id") {
                cursor.push(Value::String(last.id.clone()));
            }
            Some(cursor)
        } else {
            None
        };
        // Sort fields read only for the cursor are not part of what the caller asked for.
        for r in &mut records {
            if let Value::Object(d) = &mut r.data {
                for h in &hidden {
                    d.remove(h);
                }
            }
        }
        Ok(Page { records, next })
    }

    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        let Value::Object(obj) = &data else {
            return Err(StoreError::BadData("data must be a JSON object"));
        };
        let ent = self
            .ents
            .get(entity)
            .ok_or(StoreError::BadData("unknown entity"))?;
        if !is_uuid(scope.tenant_id()) {
            return Err(StoreError::BadData("tenant id is not a uuid"));
        }
        // The store owns the state: any incoming value is ignored and the initial state is set.
        let mut obj = obj.clone();
        if let Some(s) = &ent.state {
            obj.remove(&s.field);
        }
        let obj = &obj;
        self.check_body(entity, obj, Mode::Create)?;

        // Columns and values: engine-owned first, then caller fields, defaults, initial state.
        let mut cols = vec![
            "\"id\"".to_string(),
            "\"tenant_id\"".to_string(),
            "\"created_at\"".to_string(),
            "\"updated_at\"".to_string(),
        ];
        let mut vals = vec![
            "gen_random_uuid()".to_string(),
            "$1::text::uuid".to_string(),
            "now()".to_string(),
            "now()".to_string(),
        ];
        let mut params: Vec<Option<String>> = vec![Some(scope.tenant_id().to_string())];
        for c in ent.cols.iter().filter(|c| !c.managed) {
            let v = obj.get(&c.name).or(c.default.as_ref());
            if let Some(v) = v {
                params.push(Some(param_text(&c.ty, v)));
                cols.push(format!("\"{}\"", c.name));
                vals.push(format!("${}::text::{}", params.len(), c.cast));
            }
        }
        if let Some(s) = &ent.state {
            params.push(Some(s.initial.clone()));
            cols.push(format!("\"{}\"", s.field));
            vals.push(format!("${}::text", params.len()));
        }

        let sch = &self.schema;
        let mut c = self.lock();
        let mut tx = c.transaction().map_err(db)?;
        if let Some(key) = idempotency_key {
            let claimed = tx
                .query_opt(
                    &format!(
                        r#"INSERT INTO "{sch}"."_ce_idempotency" (tenant_id, key, entity, payload)
                           VALUES ($1::text::uuid, $2, $3, $4)
                           ON CONFLICT (tenant_id, key) DO NOTHING
                           RETURNING key"#
                    ),
                    &[&scope.tenant_id(), &key, &entity, &data],
                )
                .map_err(db)?;
            if claimed.is_none() {
                let row = tx
                    .query_one(
                        &format!(
                            r#"SELECT entity, payload, result FROM "{sch}"."_ce_idempotency"
                               WHERE tenant_id = $1::text::uuid AND key = $2"#
                        ),
                        &[&scope.tenant_id(), &key],
                    )
                    .map_err(db)?;
                let (e, payload, result): (String, Value, Option<Value>) =
                    (row.get(0), row.get(1), row.get(2));
                if e != entity || payload != data {
                    return Err(StoreError::IdempotencyKeyReused);
                }
                return replay(result);
            }
        }
        let sql = format!(
            r#"INSERT INTO "{sch}"."{entity}" ({}) VALUES ({}) RETURNING {}"#,
            cols.join(", "),
            vals.join(", "),
            ent.select_list()
        );
        let row = tx.query_one(&sql, &refs(&params)).map_err(db)?;
        let record = ent.record(entity, &row);
        if let Some(key) = idempotency_key {
            let stored = json!({"id": record.id, "entity": record.entity,
                                "version": record.version, "data": record.data});
            tx.execute(
                &format!(
                    r#"UPDATE "{sch}"."_ce_idempotency" SET result = $3
                       WHERE tenant_id = $1::text::uuid AND key = $2"#
                ),
                &[&scope.tenant_id(), &key, &stored],
            )
            .map_err(db)?;
        }
        tx.commit().map_err(db)?;
        Ok(record)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        let ent = self.ent(entity)?;
        let (tenant, id) = Self::tenant_and_id(scope, id).ok_or(StoreError::NotFound)?;
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let sql = format!(
            r#"SELECT {} FROM "{}"."{entity}" WHERE "tenant_id" = $1::text::uuid AND "id" = $2::text::uuid{live}"#,
            ent.select_list(),
            self.schema
        );
        let row = self
            .lock()
            .query_opt(&sql, &[&tenant, &id])
            .map_err(db)?
            .ok_or(StoreError::NotFound)?;
        Ok(ent.record(entity, &row))
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        let Value::Object(mut obj) = data else {
            return Err(StoreError::BadData("data must be a JSON object"));
        };
        let ent = self.ent(entity)?;
        let (tenant, id) = Self::tenant_and_id(scope, id).ok_or(StoreError::NotFound)?;
        // State and computed values are the engine's: drop whatever a read-merge-write brought.
        for c in ent.cols.iter().filter(|c| c.managed) {
            obj.remove(&c.name);
        }
        if let Some(s) = &ent.state {
            obj.remove(&s.field);
        }
        // Full replacement: the state field is not caller data, so validate without it.
        let mut check = obj.clone();
        if let Some(s) = &ent.state {
            check.remove(&s.field);
        }
        self.check_body(entity, &check, Mode::Create)?;

        let mut sets = Vec::new();
        let mut params: Vec<Option<String>> = vec![
            Some(tenant.to_string()),
            Some(id.to_string()),
            Some(expected_version.to_string()),
        ];
        for c in ent.cols.iter().filter(|c| !c.managed) {
            match obj.get(&c.name) {
                Some(v) => {
                    params.push(Some(param_text(&c.ty, v)));
                    sets.push(format!(
                        "\"{}\" = ${}::text::{}",
                        c.name,
                        params.len(),
                        c.cast
                    ));
                }
                None => sets.push(format!("\"{}\" = NULL", c.name)),
            }
        }
        sets.push("\"version\" = \"version\" + 1".into());
        sets.push("\"updated_at\" = now()".into());
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let sch = &self.schema;
        let sql = format!(
            r#"UPDATE "{sch}"."{entity}" SET {} WHERE "tenant_id" = $1::text::uuid
               AND "id" = $2::text::uuid AND "version" = $3::text::bigint{live}
               RETURNING {}"#,
            sets.join(", "),
            ent.select_list()
        );
        let mut c = self.lock();
        match c.query_opt(&sql, &refs(&params)).map_err(db)? {
            Some(row) => Ok(ent.record(entity, &row)),
            None => Err(self.missed(&mut c, ent, entity, tenant, id)?),
        }
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let ent = self.ent(entity)?;
        let (tenant, id) = Self::tenant_and_id(scope, id).ok_or(StoreError::NotFound)?;
        let sch = &self.schema;
        let version = expected_version.to_string();
        let sql = if ent.soft {
            format!(
                r#"UPDATE "{sch}"."{entity}" SET "deleted_at" = now(), "version" = "version" + 1,
                   "updated_at" = now() WHERE "tenant_id" = $1::text::uuid
                   AND "id" = $2::text::uuid AND "version" = $3::text::bigint
                   AND "deleted_at" IS NULL"#
            )
        } else {
            format!(
                r#"DELETE FROM "{sch}"."{entity}" WHERE "tenant_id" = $1::text::uuid
                   AND "id" = $2::text::uuid AND "version" = $3::text::bigint"#
            )
        };
        let mut c = self.lock();
        let n = c.execute(&sql, &[&tenant, &id, &version]).map_err(db)?;
        if n == 1 {
            Ok(())
        } else {
            Err(self.missed(&mut c, ent, entity, tenant, id)?)
        }
    }
}

impl TypedPgStore {
    /// After a guarded write matched nothing: missing, or at another version?
    fn missed(
        &self,
        c: &mut Client,
        ent: &Ent,
        entity: &str,
        tenant: &str,
        id: &str,
    ) -> Result<StoreError, StoreError> {
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let row = c
            .query_opt(
                &format!(
                    r#"SELECT "version" FROM "{}"."{entity}"
                       WHERE "tenant_id" = $1::text::uuid AND "id" = $2::text::uuid{live}"#,
                    self.schema
                ),
                &[&tenant, &id],
            )
            .map_err(db)?;
        Ok(match row {
            Some(r) => StoreError::VersionConflict {
                current: r.get::<_, i64>(0) as u64,
            },
            None => StoreError::NotFound,
        })
    }
}
