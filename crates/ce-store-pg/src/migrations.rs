//! Forward-only schema migrations for an installed schema.
//!
//! The history lives in `_ce_migration`, inside the installation's own schema. A migration is
//! a numbered SQL file embedded in the binary (`migrations/NNNN_name.sql`); its SHA-256 is
//! recorded when it is applied. Rules, all covered by `ce-server/tests/migration_*.rs`:
//!
//! 1. Versions are applied in order, one transaction each. A version that fails leaves the
//!    schema as it was and writes a `failed` row that names the version and the database error.
//! 2. Applied versions must be exactly `1..=k` with no gap, and each recorded checksum must
//!    equal the binary's. A changed checksum, a gap, or a version this binary does not know
//!    (the database is ahead of the binary) stops everything before anything is written.
//! 3. Pending SQL containing `drop` or `truncate` is refused this window, before any write.
//! 4. Nothing pending is a no-op. [`TypedPgStore::migration_plan`] reads and never writes.
//! 5. A schema installed before this history existed has no `_ce_migration`. It is refused until
//!    the operator runs [`TypedPgStore::adopt_migration_baseline`] on purpose.

use ce_store::StoreError;
use sha2::{Digest, Sha256};

use crate::typed::{db, ident_ok, TypedPgStore};

/// One numbered migration. `sql` may use `__SCHEMA__` for the installation's schema name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    pub version: u32,
    pub name: String,
    pub sql: String,
}

impl Migration {
    pub fn new(version: u32, name: &str, sql: &str) -> Migration {
        Migration {
            version,
            name: name.to_string(),
            sql: sql.to_string(),
        }
    }

    /// Hex SHA-256 of the SQL text exactly as embedded.
    pub fn checksum(&self) -> String {
        hex(&Sha256::digest(self.sql.as_bytes()))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The migrations this binary ships, in order.
pub fn embedded() -> Vec<Migration> {
    vec![
        Migration::new(
            1,
            "baseline",
            include_str!("../migrations/0001_baseline.sql"),
        ),
        Migration::new(
            2,
            "migration_provenance",
            include_str!("../migrations/0002_migration_provenance.sql"),
        ),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// The binary's own list is not 1, 2, 3 ... in order.
    BadList(String),
    /// The schema has no `_ce_migration` history; adopt the baseline on purpose first.
    NoHistory,
    /// The database holds a version below some other applied version.
    Gap { missing: u32 },
    /// An applied migration's recorded checksum is not the binary's.
    ChecksumChanged { version: u32 },
    /// The database has applied a version this binary does not contain.
    AheadOfBinary { database: u32, binary: u32 },
    /// A pending migration contains prohibited destructive SQL.
    Destructive { version: u32 },
    /// A migration failed; it was rolled back and the failure was recorded.
    Failed { version: u32, message: String },
    /// Anything else (connection, bad schema name).
    Store(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::BadList(m) => write!(f, "the binary's migration list is invalid: {m}"),
            MigrateError::NoHistory => write!(
                f,
                "this schema has no migration history (run `ce-serve migrate --adopt` once)"
            ),
            MigrateError::Gap { missing } => {
                write!(
                    f,
                    "migration history has a gap: version {missing} is missing"
                )
            }
            MigrateError::ChecksumChanged { version } => write!(
                f,
                "migration {version} was applied with different SQL than this binary carries"
            ),
            MigrateError::AheadOfBinary { database, binary } => write!(
                f,
                "the database is at version {database} but this binary only knows version {binary}"
            ),
            MigrateError::Destructive { version } => write!(
                f,
                "migration {version} contains destructive SQL, which is not allowed in this window"
            ),
            MigrateError::Failed { version, message } => {
                write!(
                    f,
                    "migration {version} failed and was rolled back: {message}"
                )
            }
            MigrateError::Store(m) => write!(f, "{m}"),
        }
    }
}

impl From<StoreError> for MigrateError {
    fn from(e: StoreError) -> Self {
        MigrateError::Store(format!("{e:?}"))
    }
}

/// What a run would do (or did).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Highest applied version, 0 when none.
    pub current: u32,
    /// Versions still to apply, in order.
    pub pending: Vec<(u32, String)>,
    /// The schema-expanded SQL of each pending version, in the same order, for review.
    pub statements: Vec<String>,
}

/// One row of the history, for display and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub version: u32,
    pub name: String,
    pub sha256: String,
    pub status: String,
    pub applied_by: String,
    pub manifest_hash: String,
}

/// `CREATE TABLE` for the history, shared by install and adopt.
pub(crate) fn history_ddl(schema: &str) -> String {
    format!(
        r#"CREATE TABLE "{schema}"."_ce_migration" (
               id            bigserial   PRIMARY KEY,
               version       integer     NOT NULL CHECK (version > 0),
               name          text        NOT NULL,
               sha256        text        NOT NULL,
               applied_at    timestamptz NOT NULL DEFAULT now(),
               applied_by    text        NOT NULL,
               manifest_hash text        NOT NULL,
               duration_ms   bigint      NOT NULL DEFAULT 0,
               status        text        NOT NULL CHECK (status IN ('applied', 'failed')),
               error         text
           );
           CREATE UNIQUE INDEX "_ce_migration_applied_version"
               ON "{schema}"."_ce_migration" (version) WHERE status = 'applied'"#
    )
}

fn check_list(list: &[Migration]) -> Result<(), MigrateError> {
    for (i, m) in list.iter().enumerate() {
        if m.version as usize != i + 1 {
            return Err(MigrateError::BadList(format!(
                "expected version {} at position {}, found {}",
                i + 1,
                i + 1,
                m.version
            )));
        }
    }
    Ok(())
}

/// Removes SQL comments (`-- ...` and `/* ... */`, which nest in Postgres) without touching
/// quoted text: a `--` inside a string literal, quoted identifier or dollar-quoted body is not a
/// comment. Everything that is not a real comment is kept, including string contents, so the
/// destructive scan below errs on the side of refusing.
fn strip_comments(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'-' && next == Some(b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
        } else if c == b'\'' || c == b'"' {
            // A quoted string or identifier; a doubled quote is an escaped quote.
            let q = c;
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == q {
                    if b.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&sql[start..i]);
        } else if c == b'$' {
            // A dollar-quoted body: $tag$ ... $tag$ (the tag may be empty).
            let rest = &sql[i..];
            let tag_len = rest[1..]
                .find('$')
                .filter(|n| {
                    rest[1..=*n]
                        .bytes()
                        .all(|x| x.is_ascii_alphanumeric() || x == b'_')
                })
                .map(|n| n + 2);
            match tag_len {
                Some(n) => {
                    let tag = &rest[..n];
                    let end = rest[n..]
                        .find(tag)
                        .map(|e| i + n + e + n)
                        .unwrap_or(sql.len());
                    out.push_str(&sql[i..end]);
                    i = end;
                }
                None => {
                    out.push('$');
                    i += 1;
                }
            }
        } else {
            let ch = sql[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// True when the SQL contains prohibited destructive statements (`drop ...`, `truncate`).
/// Real comments are ignored; the match is on whole words, so a column named `dropped_at` is
/// fine. Quoted text is scanned too: this guard refuses when unsure.
pub fn is_destructive(sql: &str) -> bool {
    let lower = strip_comments(sql).to_ascii_lowercase();
    let mut word = String::new();
    let mut words: Vec<String> = Vec::new();
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words.iter().any(|w| w == "drop" || w == "truncate")
}

impl TypedPgStore {
    fn history_exists(&self) -> Result<bool, StoreError> {
        let row = self
            .lock()?
            .query_one(
                "SELECT to_regclass($1) IS NOT NULL",
                &[&format!(r#""{}"."_ce_migration""#, self.schema)],
            )
            .map_err(db)?;
        Ok(row.get(0))
    }

    /// Applied versions, ascending.
    fn applied(&self) -> Result<Vec<(u32, String)>, StoreError> {
        let rows = self
            .lock()?
            .query(
                &format!(
                    r#"SELECT version, sha256 FROM "{}"."_ce_migration"
                       WHERE status = 'applied' ORDER BY version"#,
                    self.schema
                ),
                &[],
            )
            .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, i32>(0) as u32, r.get::<_, String>(1)))
            .collect())
    }

    /// The whole history, applied and failed, in the order it happened.
    pub fn migration_history(&self) -> Result<Vec<HistoryRow>, MigrateError> {
        let rows = self
            .lock()?
            .query(
                &format!(
                    r#"SELECT version, name, sha256, status, applied_by, manifest_hash
                       FROM "{}"."_ce_migration" ORDER BY id"#,
                    self.schema
                ),
                &[],
            )
            .map_err(|e| MigrateError::from(db(e)))?;
        Ok(rows
            .iter()
            .map(|r| HistoryRow {
                version: r.get::<_, i32>(0) as u32,
                name: r.get(1),
                sha256: r.get(2),
                status: r.get(3),
                applied_by: r.get(4),
                manifest_hash: r.get(5),
            })
            .collect())
    }

    /// Checks the history against `list` and returns what is pending. Writes nothing.
    pub fn migration_plan(&self, list: &[Migration]) -> Result<Plan, MigrateError> {
        check_list(list)?;
        if !ident_ok(&self.schema) {
            return Err(MigrateError::Store("invalid schema name".into()));
        }
        if !self.history_exists()? {
            return Err(MigrateError::NoHistory);
        }
        let applied = self.applied()?;
        for (i, (v, _)) in applied.iter().enumerate() {
            let want = i as u32 + 1;
            if *v != want {
                return Err(MigrateError::Gap { missing: want });
            }
        }
        let highest = applied.last().map(|a| a.0).unwrap_or(0);
        let binary = list.last().map(|m| m.version).unwrap_or(0);
        if highest > binary {
            return Err(MigrateError::AheadOfBinary {
                database: highest,
                binary,
            });
        }
        for (v, sum) in &applied {
            if list[*v as usize - 1].checksum() != *sum {
                return Err(MigrateError::ChecksumChanged { version: *v });
            }
        }
        let pending: Vec<(u32, String)> = list
            .iter()
            .filter(|m| m.version > highest)
            .map(|m| (m.version, m.name.clone()))
            .collect();
        for m in list.iter().filter(|m| m.version > highest) {
            if is_destructive(&m.sql) {
                return Err(MigrateError::Destructive { version: m.version });
            }
        }
        let statements = list
            .iter()
            .filter(|m| m.version > highest)
            .map(|m| m.sql.replace("__SCHEMA__", &self.schema))
            .collect();
        Ok(Plan {
            current: highest,
            pending,
            statements,
        })
    }

    /// Applies every pending migration in order, one transaction each, and records them.
    /// Returns what it applied (empty when the schema is current).
    pub fn migrate(&self, list: &[Migration], applied_by: &str) -> Result<Plan, MigrateError> {
        let plan = self.migration_plan(list)?;
        let mut done = Vec::new();
        for m in list.iter().filter(|m| m.version > plan.current) {
            self.apply_one(m, applied_by)?;
            done.push((m.version, m.name.clone()));
        }
        Ok(Plan {
            current: plan.current,
            pending: done,
            statements: plan.statements,
        })
    }

    fn apply_one(&self, m: &Migration, applied_by: &str) -> Result<(), MigrateError> {
        let sql = m.sql.replace("__SCHEMA__", &self.schema);
        let started = std::time::Instant::now();
        let mut conn = self.lock()?;
        let outcome = (|| -> Result<(), postgres::Error> {
            let mut tx = conn.transaction()?;
            // One migrator at a time per schema. A second run waits here, then sees the version
            // already applied and skips it instead of repeating the DDL and recording a failure.
            tx.execute(
                "SELECT pg_advisory_xact_lock(hashtext($1))",
                &[&format!("{}/_ce_migration", self.schema)],
            )?;
            let already = tx.query_opt(
                &format!(
                    r#"SELECT 1 FROM "{}"."_ce_migration"
                       WHERE version = $1 AND status = 'applied'"#,
                    self.schema
                ),
                &[&(m.version as i32)],
            )?;
            if already.is_some() {
                return tx.commit();
            }
            tx.batch_execute(&sql)?;
            tx.execute(
                &format!(
                    r#"INSERT INTO "{}"."_ce_migration"
                       (version, name, sha256, applied_by, manifest_hash, duration_ms, status)
                       VALUES ($1, $2, $3, $4, $5, $6, 'applied')"#,
                    self.schema
                ),
                &[
                    &(m.version as i32),
                    &m.name,
                    &m.checksum(),
                    &applied_by,
                    &self.manifest.hash,
                    &(started.elapsed().as_millis() as i64),
                ],
            )?;
            tx.commit()
        })();
        let Err(e) = outcome else { return Ok(()) };
        let message = e
            .as_db_error()
            .map(|d| d.message().to_string())
            .unwrap_or_else(|| e.to_string());
        // The failed attempt is rolled back; this separate write is the diagnosis.
        let _ = conn.execute(
            &format!(
                r#"INSERT INTO "{}"."_ce_migration"
                   (version, name, sha256, applied_by, manifest_hash, duration_ms, status, error)
                   VALUES ($1, $2, $3, $4, $5, $6, 'failed', $7)"#,
                self.schema
            ),
            &[
                &(m.version as i32),
                &m.name,
                &m.checksum(),
                &applied_by,
                &self.manifest.hash,
                &(started.elapsed().as_millis() as i64),
                &message,
            ],
        );
        Err(MigrateError::Failed {
            version: m.version,
            message,
        })
    }

    /// For a schema installed before migration history existed: creates `_ce_migration` and
    /// records version 1 (the baseline) only. Later versions are then applied by `migrate`.
    /// Refuses when history already exists.
    pub fn adopt_migration_baseline(
        &self,
        list: &[Migration],
        applied_by: &str,
    ) -> Result<(), MigrateError> {
        check_list(list)?;
        if self.history_exists()? {
            return Err(MigrateError::Store(
                "migration history already exists; nothing to adopt".into(),
            ));
        }
        let base = list
            .first()
            .ok_or_else(|| MigrateError::BadList("empty".into()))?;
        let mut conn = self.lock()?;
        let run = (|| -> Result<(), postgres::Error> {
            let mut tx = conn.transaction()?;
            tx.batch_execute(&history_ddl(&self.schema))?;
            tx.execute(
                &format!(
                    r#"INSERT INTO "{}"."_ce_migration"
                       (version, name, sha256, applied_by, manifest_hash, status)
                       VALUES ($1, $2, $3, $4, $5, 'applied')"#,
                    self.schema
                ),
                &[
                    &(base.version as i32),
                    &base.name,
                    &base.checksum(),
                    &applied_by,
                    &self.manifest.hash,
                ],
            )?;
            tx.commit()
        })();
        run.map_err(|e| MigrateError::from(db(e)))
    }
}
