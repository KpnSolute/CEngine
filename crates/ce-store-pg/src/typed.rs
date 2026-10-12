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
//! Computed columns are stored like any other: the host evaluates them first
//! (`ce_validate::evaluate_computed`) and passes them in the data. A direct caller that leaves a
//! required computed field out gets a `required` constraint error.
//! On update the state field in the incoming data is ignored, so a read-merge-write cycle
//! cannot move state.

use crate::pool::{Conn, Pool};
use ce_manifest::Manifest;
use ce_query::{plan_list, ListRequest, Param, Target};
use ce_store::{Page, Record, Store, StoreError, TenantScope};
use ce_validate::{validate, Mode};
use postgres::{types::ToSql, GenericClient};
use serde_json::{json, Map, Value};
use std::{cell::RefCell, collections::HashMap};

pub use crate::pool::PoolConfig;

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
    /// The declared state field: never written from caller data. Computed values arrive in the
    /// data already evaluated by the host (callers cannot set them; validation refuses that).
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
    pool: std::sync::Arc<Pool>,
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
        Self::install_with(url, m, schema, PoolConfig::default())
    }

    /// Like [`install`](Self::install), with explicit pool limits.
    pub fn install_with(
        url: &str,
        m: &Manifest,
        schema: &str,
        cfg: PoolConfig,
    ) -> Result<Self, StoreError> {
        let ddl = ce_migrate::generate(m, schema)
            .map_err(|_| StoreError::BadData("manifest cannot be installed"))?;
        let pool = Self::pool(url, cfg)?;
        let mut client = pool.get()?;
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
        tx.batch_execute(&crate::blobs::ddl(schema)).map_err(db)?;
        tx.batch_execute(&crate::staff::ddl(schema)).map_err(db)?;
        tx.commit().map_err(db)?;
        drop(client);
        Self::build(pool, m, schema)
    }

    /// Connects to an installed schema. Does not change the database.
    pub fn open(url: &str, m: &Manifest, schema: &str) -> Result<Self, StoreError> {
        Self::open_with(url, m, schema, PoolConfig::default())
    }

    /// Like [`open`](Self::open), with explicit pool limits. Fails if the database cannot be
    /// reached.
    pub fn open_with(
        url: &str,
        m: &Manifest,
        schema: &str,
        cfg: PoolConfig,
    ) -> Result<Self, StoreError> {
        let pool = Self::pool(url, cfg)?;
        drop(pool.get()?);
        Self::build(pool, m, schema)
    }

    fn pool(url: &str, cfg: PoolConfig) -> Result<Pool, StoreError> {
        let url = url.to_string();
        let connector = crate::tls::Connector::new(&cfg.tls)?;
        Ok(Pool::new(
            Box::new(move || connector.connect(&url).map_err(db)),
            cfg,
        ))
    }

    fn build(pool: Pool, m: &Manifest, schema: &str) -> Result<Self, StoreError> {
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
                    managed: state.as_ref().is_some_and(|s| s.field == *fname),
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
            pool: std::sync::Arc::new(pool),
            schema: schema.to_string(),
            manifest: m.clone(),
            ents,
        })
    }

    pub fn schema_name(&self) -> &str {
        &self.schema
    }

    /// Deletes stored file bytes no record points at any more and that are at least
    /// `older_than_secs` old (the age protects an upload whose record write is still in flight).
    /// Counts soft-deleted records as pointing. The old bytes of a replaced file are also gone from
    /// audit and changeset history afterwards, so a revert of that replacement can no longer
    /// restore them. Returns how many were deleted.
    pub fn sweep_blobs(&self, older_than_secs: u64) -> Result<u64, StoreError> {
        let mut unreferenced = String::new();
        for (name, ent) in &self.ents {
            for c in ent.cols.iter().filter(|c| c.ty == "file") {
                unreferenced.push_str(&format!(
                    r#" AND NOT EXISTS (SELECT 1 FROM "{schema}"."{name}" t
                          WHERE t.tenant_id = b.tenant_id AND t."{col}"->>'key' = b.key)"#,
                    schema = self.schema,
                    col = c.name,
                ));
            }
        }
        let sql = format!(
            r#"DELETE FROM "{schema}"."_ce_blobs" b
               WHERE b.created_at < now() - ($1::bigint * interval '1 second'){unreferenced}"#,
            schema = self.schema,
        );
        let n = self
            .lock()?
            .execute(&sql, &[&(older_than_secs as i64)])
            .map_err(db)?;
        Ok(n)
    }

    /// Staff accounts for the installation's one tenant, kept in this schema. Creates the tables
    /// when they are missing.
    pub fn staff(&self, tenant: &str) -> Result<crate::PgStaff, crate::StaffError> {
        crate::staff::PgStaff::open(self.pool.clone(), &self.schema, tenant)
    }

    /// File bytes kept in this installation's schema, sharing the store's connections. Creates the
    /// table when an installation made before files existed does not have it yet.
    pub fn blobs(&self) -> Result<crate::PgBlobs, StoreError> {
        crate::PgBlobs::open(self.pool.clone(), &self.schema)
    }

    /// Drops the schema and everything in it. For tests.
    pub fn drop_schema(&self) -> Result<(), StoreError> {
        self.lock()?
            .batch_execute(&format!(
                r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#,
                self.schema
            ))
            .map_err(db)
    }

    fn lock(&self) -> Result<Conn<'_>, StoreError> {
        self.pool.get()
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
        // Computed values come from the host's evaluator, not a caller, so they are not
        // re-checked as caller input (validation refuses callers that set them).
        let fields = &self.manifest.value["entities"][entity]["fields"];
        let mut caller = data.clone();
        caller.retain(|k, _| !fields[k.as_str()]["computed"].is_string());
        validate(&self.manifest, entity, &caller, mode)
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

impl TypedPgStore {
    fn aggregate_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        req: &ce_query::AggRequest,
    ) -> Result<Vec<Map<String, Value>>, StoreError> {
        use ce_query::{plan_aggregate, ColKind};
        if !self.ents.contains_key(entity) {
            return Err(StoreError::BadData("unknown entity"));
        }
        let target = Target {
            schema: self.schema.clone(),
            entity: entity.to_string(),
        };
        let planned =
            plan_aggregate(&self.manifest, &target, scope, req).map_err(StoreError::Query)?;
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
        let rows = c.query(planned.sql.as_str(), &refs).map_err(db)?;
        Ok(rows
            .iter()
            .map(|row| {
                let mut out = Map::new();
                for (i, (name, kind)) in planned.columns.iter().enumerate() {
                    let v = match kind {
                        ColKind::Int => row
                            .get::<_, Option<i64>>(i)
                            .map_or(Value::Null, Value::from),
                        ColKind::Dec => row
                            .get::<_, Option<String>>(i)
                            .map_or(Value::Null, Value::String),
                        ColKind::Json => row.get::<_, Option<Value>>(i).unwrap_or(Value::Null),
                    };
                    out.insert(name.clone(), v);
                }
                out
            })
            .collect())
    }

    fn list_on(
        &self,
        c: &mut impl GenericClient,
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
        let rows = c.query(planned.sql.as_str(), &refs).map_err(db)?;
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

    /// Claims `key` for this request, comparing `data` as the client sent it. Returns the first
    /// result if the key was already used for the same request (a replay), an error if it was
    /// used for a different one, and `None` when the key is now ours (or there is no key).
    fn claim_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        data: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<Option<Record>, StoreError> {
        if !data.is_object() {
            return Err(StoreError::BadData("data must be a JSON object"));
        }
        self.ents
            .get(entity)
            .ok_or(StoreError::BadData("unknown entity"))?;
        if !is_uuid(scope.tenant_id()) {
            return Err(StoreError::BadData("tenant id is not a uuid"));
        }
        let sch = &self.schema;
        if let Some(key) = idempotency_key {
            let claimed = c
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
                let row = c
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
                if e != entity || &payload != data {
                    return Err(StoreError::IdempotencyKeyReused);
                }
                return replay(result).map(Some);
            }
        }
        Ok(None)
    }

    /// Writes the record row (and, with a key, stores the result against it). `body` is the
    /// caller's fields with any derived ones merged in.
    fn insert_row_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        body: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        let ent = self
            .ents
            .get(entity)
            .ok_or(StoreError::BadData("unknown entity"))?;
        let sch = &self.schema;
        // The store owns the state: any incoming value is ignored and the initial state is set.
        let mut obj = body.as_object().cloned().unwrap_or_default();
        if let Some(s) = &ent.state {
            obj.remove(&s.field);
        }
        // A caller-chosen id (validated by the host) is stored as given; it must be unused.
        let chosen = obj.remove("id").and_then(|v| v.as_str().map(String::from));
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
        if let Some(id) = chosen {
            params.push(Some(id));
            vals[0] = format!("${}::text::uuid", params.len());
        }

        let sql = format!(
            r#"INSERT INTO "{sch}"."{entity}" ({}) VALUES ({}) RETURNING {}"#,
            cols.join(", "),
            vals.join(", "),
            ent.select_list()
        );
        let row = c.query_one(&sql, &refs(&params)).map_err(db)?;
        let record = ent.record(entity, &row);
        if let Some(key) = idempotency_key {
            let stored = json!({"id": record.id, "entity": record.entity,
                                "version": record.version, "data": record.data});
            c.execute(
                &format!(
                    r#"UPDATE "{sch}"."_ce_idempotency" SET result = $3
                       WHERE tenant_id = $1::text::uuid AND key = $2"#
                ),
                &[&scope.tenant_id(), &key, &stored],
            )
            .map_err(db)?;
        }
        Ok(record)
    }

    fn insert_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        if let Some(first) = self.claim_on(c, scope, entity, &data, idempotency_key)? {
            return Ok(first);
        }
        self.insert_row_on(c, scope, entity, data, idempotency_key)
    }

    fn get_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        lock: bool,
    ) -> Result<Record, StoreError> {
        let ent = self.ent(entity)?;
        let (tenant, id) = Self::tenant_and_id(scope, id).ok_or(StoreError::NotFound)?;
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let sql = format!(
            r#"SELECT {} FROM "{}"."{entity}" WHERE "tenant_id" = $1::text::uuid AND "id" = $2::text::uuid{live}{}"#,
            ent.select_list(),
            self.schema,
            if lock { " FOR NO KEY UPDATE" } else { "" }
        );
        let row = c
            .query_opt(&sql, &[&tenant, &id])
            .map_err(db)?
            .ok_or(StoreError::NotFound)?;
        Ok(ent.record(entity, &row))
    }

    fn update_on(
        &self,
        c: &mut impl GenericClient,
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
        match c.query_opt(&sql, &refs(&params)).map_err(db)? {
            Some(row) => Ok(ent.record(entity, &row)),
            None => Err(self.missed(c, ent, entity, tenant, id)?),
        }
    }

    fn set_state_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        state: &str,
    ) -> Result<Record, StoreError> {
        let ent = self.ent(entity)?;
        let field = &ent.state.as_ref().ok_or(StoreError::Unsupported)?.field;
        let (tenant, id) = Self::tenant_and_id(scope, id).ok_or(StoreError::NotFound)?;
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let sch = &self.schema;
        let sql = format!(
            r#"UPDATE "{sch}"."{entity}" SET "{field}" = $4::text, "version" = "version" + 1,
               "updated_at" = now() WHERE "tenant_id" = $1::text::uuid
               AND "id" = $2::text::uuid AND "version" = $3::text::bigint{live}
               RETURNING {}"#,
            ent.select_list()
        );
        let params = vec![
            Some(tenant.to_string()),
            Some(id.to_string()),
            Some(expected_version.to_string()),
            Some(state.to_string()),
        ];
        match c.query_opt(&sql, &refs(&params)).map_err(db)? {
            Some(row) => Ok(ent.record(entity, &row)),
            None => Err(self.missed(c, ent, entity, tenant, id)?),
        }
    }

    fn delete_on(
        &self,
        c: &mut impl GenericClient,
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
        let n = c.execute(&sql, &[&tenant, &id, &version]).map_err(db)?;
        if n == 1 {
            Ok(())
        } else {
            Err(self.missed(c, ent, entity, tenant, id)?)
        }
    }

    fn child_values_on(
        &self,
        c: &mut impl GenericClient,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        let ent = self
            .ents
            .get(entity)
            .ok_or(StoreError::BadData("unknown entity"))?;
        if !ent
            .cols
            .iter()
            .any(|c| c.name == ref_field && c.ty == "ref")
        {
            return Err(StoreError::BadData("not a ref field of this entity"));
        }
        if attr.is_some_and(|a| !ent.cols.iter().any(|c| c.name == a)) {
            return Err(StoreError::BadData("unknown field"));
        }
        // An id that is not a UUID points at nothing.
        let Some((tenant, parent)) = Self::tenant_and_id(scope, parent_id) else {
            return Ok(Vec::new());
        };
        let live = if ent.soft {
            r#" AND "deleted_at" IS NULL"#
        } else {
            ""
        };
        let sql = format!(
            r#"SELECT {} FROM "{}"."{entity}" WHERE "tenant_id" = $1::text::uuid
               AND "{ref_field}" = $2::text::uuid{live}"#,
            ent.select_list(),
            self.schema
        );
        let rows = c.query(&sql, &[&tenant, &parent]).map_err(db)?;
        Ok(rows
            .iter()
            .map(|row| match attr {
                Some(a) => ent
                    .record(entity, row)
                    .data
                    .get(a)
                    .cloned()
                    .unwrap_or(Value::Null),
                None => Value::Bool(true),
            })
            .collect())
    }
}

impl Store for TypedPgStore {
    fn ping(&self) -> Result<(), StoreError> {
        let mut c = self.lock()?;
        c.batch_execute("SELECT 1").map_err(db)
    }

    fn aggregate(
        &self,
        scope: &TenantScope,
        entity: &str,
        req: &ce_query::AggRequest,
    ) -> Result<Vec<Map<String, Value>>, StoreError> {
        let mut c = self.lock()?;
        self.aggregate_on(&mut *c, scope, entity, req)
    }

    fn list(
        &self,
        scope: &TenantScope,
        entity: &str,
        req: &ListRequest,
    ) -> Result<Page, StoreError> {
        let mut c = self.lock()?;
        self.list_on(&mut *c, scope, entity, req)
    }

    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        let mut c = self.lock()?;
        let mut tx = c.transaction().map_err(db)?;
        let record = self.insert_on(&mut tx, scope, entity, data, idempotency_key)?;
        tx.commit().map_err(db)?;
        Ok(record)
    }

    fn insert_with(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: Value,
        derive: &mut dyn FnMut() -> Result<Map<String, Value>, StoreError>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        let mut c = self.lock()?;
        let mut tx = c.transaction().map_err(db)?;
        if let Some(first) = self.claim_on(&mut tx, scope, entity, &caller, idempotency_key)? {
            return Ok(first);
        }
        let mut body = caller;
        if let Value::Object(o) = &mut body {
            o.extend(derive()?);
        }
        let record = self.insert_row_on(&mut tx, scope, entity, body, idempotency_key)?;
        tx.commit().map_err(db)?;
        Ok(record)
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        let mut c = self.lock()?;
        self.get_on(&mut *c, scope, entity, id, false)
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        let mut c = self.lock()?;
        self.update_on(&mut *c, scope, entity, id, expected_version, data)
    }

    fn set_state(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        _field: &str,
        state: &str,
    ) -> Result<Record, StoreError> {
        let mut c = self.lock()?;
        self.set_state_on(&mut *c, scope, entity, id, expected_version, state)
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        let mut c = self.lock()?;
        self.delete_on(&mut *c, scope, entity, id, expected_version)
    }

    fn child_values(
        &self,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        let mut c = self.lock()?;
        self.child_values_on(&mut *c, scope, entity, ref_field, parent_id, attr)
    }

    /// One database transaction on the store's connection, held for the whole closure.
    fn atomic(
        &self,
        f: &mut dyn FnMut(&dyn Store) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut c = self.lock()?;
        let tx = TypedTx {
            store: self,
            tx: RefCell::new(c.transaction().map_err(db)?),
        };
        // An error drops the transaction, which rolls it back.
        f(&tx)?;
        tx.tx.into_inner().commit().map_err(db)
    }
}

/// The store a closure sees inside `TypedPgStore::atomic`: every call runs in the one open
/// transaction. `get_locked` takes a row lock, held until the transaction ends.
struct TypedTx<'a> {
    store: &'a TypedPgStore,
    tx: RefCell<postgres::Transaction<'a>>,
}

impl Store for TypedTx<'_> {
    fn ping(&self) -> Result<(), StoreError> {
        Ok(())
    }

    fn list(
        &self,
        scope: &TenantScope,
        entity: &str,
        req: &ListRequest,
    ) -> Result<Page, StoreError> {
        self.store
            .list_on(&mut *self.tx.borrow_mut(), scope, entity, req)
    }

    fn insert(
        &self,
        scope: &TenantScope,
        entity: &str,
        data: Value,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        self.store.insert_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            data,
            idempotency_key,
        )
    }

    fn insert_with(
        &self,
        scope: &TenantScope,
        entity: &str,
        caller: Value,
        derive: &mut dyn FnMut() -> Result<Map<String, Value>, StoreError>,
        idempotency_key: Option<&str>,
    ) -> Result<Record, StoreError> {
        // `derive` reads through this same transaction, so the borrow is released around it.
        let claimed = {
            let mut tx = self.tx.borrow_mut();
            self.store
                .claim_on(&mut *tx, scope, entity, &caller, idempotency_key)?
        };
        if let Some(first) = claimed {
            return Ok(first);
        }
        let mut body = caller;
        if let Value::Object(o) = &mut body {
            o.extend(derive()?);
        }
        self.store.insert_row_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            body,
            idempotency_key,
        )
    }

    fn get(&self, scope: &TenantScope, entity: &str, id: &str) -> Result<Record, StoreError> {
        self.store
            .get_on(&mut *self.tx.borrow_mut(), scope, entity, id, false)
    }

    fn get_locked(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
    ) -> Result<Record, StoreError> {
        self.store
            .get_on(&mut *self.tx.borrow_mut(), scope, entity, id, true)
    }

    fn update(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        data: Value,
    ) -> Result<Record, StoreError> {
        self.store.update_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            id,
            expected_version,
            data,
        )
    }

    fn set_state(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
        _field: &str,
        state: &str,
    ) -> Result<Record, StoreError> {
        self.store.set_state_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            id,
            expected_version,
            state,
        )
    }

    fn delete(
        &self,
        scope: &TenantScope,
        entity: &str,
        id: &str,
        expected_version: u64,
    ) -> Result<(), StoreError> {
        self.store.delete_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            id,
            expected_version,
        )
    }

    fn child_values(
        &self,
        scope: &TenantScope,
        entity: &str,
        ref_field: &str,
        parent_id: &str,
        attr: Option<&str>,
    ) -> Result<Vec<Value>, StoreError> {
        self.store.child_values_on(
            &mut *self.tx.borrow_mut(),
            scope,
            entity,
            ref_field,
            parent_id,
            attr,
        )
    }

    fn atomic(
        &self,
        f: &mut dyn FnMut(&dyn Store) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        f(self)
    }
}

impl TypedPgStore {
    /// After a guarded write matched nothing: missing, or at another version?
    fn missed(
        &self,
        c: &mut impl GenericClient,
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
