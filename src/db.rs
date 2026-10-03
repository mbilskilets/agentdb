use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params, params_from_iter};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::change::{Change, ChangeKind};
use crate::error::{DbError, Result};
use crate::query::Query;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ask::{self, Asked};
use crate::jev::Judge;
use crate::schema::{Field, FieldType, TableDef, closest, format_utc};

const SETUP: &str = "
CREATE TABLE IF NOT EXISTS _tables (
    name TEXT PRIMARY KEY,
    schema TEXT NOT NULL,
    next_id INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS docs (
    tbl TEXT NOT NULL,
    id INTEGER NOT NULL,
    version INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (tbl, id)
);
CREATE TABLE IF NOT EXISTS changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    tbl TEXT NOT NULL,
    kind TEXT NOT NULL,
    at TEXT NOT NULL,
    doc TEXT NOT NULL
);
";
/// Bumped whenever the layout of the storage tables changes.
const FORMAT_VERSION: i64 = 1;
const DOC_COLUMNS: &str = "id, version, created_at, updated_at, body";
const MAX_CHANGES: i64 = 500;

/// A stored document. `id`, `version`, `created_at` and `updated_at` are set
/// by the database; everything else is the caller's fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doc {
    pub id: i64,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// One page of query results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    pub docs: Vec<Doc>,
    /// How many documents match the filters in total, across all pages.
    pub total: i64,
    /// Pass this as the next query's offset to continue. `None` on the last page.
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub description: Option<String>,
    pub count: i64,
    pub fields: Vec<Field>,
}

/// One tenant's database. Cheap to share between threads behind an `Arc`.
#[derive(Debug)]
pub struct AgentDb {
    conn: Mutex<Connection>,
    subscribers: Mutex<Vec<Sender<Change>>>,
    frozen_time: Mutex<Option<OffsetDateTime>>,
}

impl AgentDb {
    /// Opens the database file at `path`, creating it if needed. The whole
    /// file is encrypted with `key`.
    ///
    /// # Errors
    /// [`DbError::EmptyKey`] for an empty key, [`DbError::WrongKey`] when the key
    /// does not match an existing file.
    pub fn open(path: impl AsRef<Path>, key: &str) -> Result<Self> {
        if key.is_empty() {
            return Err(DbError::EmptyKey);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "key", key)?;
        // The key is only tested when the file is first read.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| match error.sqlite_error_code() {
            Some(ErrorCode::NotADatabase) => DbError::WrongKey,
            _ => DbError::Storage(error),
        })?;
        Self::init(conn)
    }

    /// Opens an unencrypted database that lives only in memory.
    ///
    /// # Errors
    /// Fails only if the storage engine cannot start.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > FORMAT_VERSION {
            return Err(DbError::NewerFormat {
                found,
                supported: FORMAT_VERSION,
            });
        }
        conn.execute_batch(SETUP)?;
        conn.pragma_update(None, "user_version", FORMAT_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
            subscribers: Mutex::new(Vec::new()),
            frozen_time: Mutex::new(None),
        })
    }

    /// Makes the database behave as if the current time were `at` (an RFC 3339
    /// timestamp), or returns to the real clock with `None`. For tests and
    /// evals that need documents created "yesterday".
    ///
    /// # Errors
    /// [`DbError::InvalidTimestamp`] when `at` is not an RFC 3339 timestamp.
    pub fn freeze_time(&self, at: Option<&str>) -> Result<()> {
        let moment = at
            .map(|text| {
                OffsetDateTime::parse(text, &Rfc3339).map_err(|error| DbError::InvalidTimestamp {
                    got: text.to_owned(),
                    reason: error.to_string(),
                })
            })
            .transpose()?;
        *self
            .frozen_time
            .lock()
            .map_err(|error| DbError::Internal(error.to_string()))? = moment;
        Ok(())
    }

    fn now(&self) -> Result<OffsetDateTime> {
        let frozen = *self
            .frozen_time
            .lock()
            .map_err(|error| DbError::Internal(error.to_string()))?;
        Ok(frozen.unwrap_or_else(OffsetDateTime::now_utc))
    }

    pub(crate) fn timestamp(&self) -> Result<String> {
        format_utc(self.now()?)
            .ok_or_else(|| DbError::Internal("could not format the current time".to_owned()))
    }

    /// Answers a request written in English, such as "clients created today".
    /// The request is turned into a [`Query`]; it runs only when the model is
    /// confident and the request is a plain read. The returned [`Asked`]
    /// always says how the request was understood.
    ///
    /// # Errors
    /// Fails when the model cannot be reached or storage fails.
    pub fn ask(&self, judge: &dyn Judge, text: &str) -> Result<Asked> {
        let defs = all_defs(&*self.lock()?)?;
        let plan = ask::plan(&defs, text, self.now()?.date(), judge)?;
        let page = match (&plan.query, &plan.refusal) {
            (Some(query), None) => Some(self.find(query)?),
            _ => None,
        };
        Ok(Asked {
            query: plan.query,
            confidence: plan.confidence,
            refusal: plan.refusal,
            page,
            usage: plan.usage,
        })
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|error| DbError::Internal(error.to_string()))
    }

    /// Lists every table with its fields and document count. An agent's first
    /// call: it shows everything needed to read and write.
    ///
    /// # Errors
    /// Fails only on a storage error.
    pub fn describe(&self) -> Result<Vec<TableInfo>> {
        let conn = self.lock()?;
        all_defs(&conn)?
            .into_iter()
            .map(|def| {
                Ok(TableInfo {
                    count: count_docs(&conn, &def.name)?,
                    name: def.name,
                    description: def.description,
                    fields: def.fields,
                })
            })
            .collect()
    }

    /// Stores a new document and returns it with its assigned `id`.
    ///
    /// # Errors
    /// Fails when the document does not match the table's schema or points to
    /// a document that does not exist.
    pub fn insert(&self, table: &str, doc: Value) -> Result<Doc> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let def = load_def(&tx, table)?;
        let mut fields = def.check(into_object(doc)?)?;
        fields.retain(|_, value| !value.is_null());
        def.check_required(&fields)?;
        check_references(&tx, &def, &fields)?;
        let id: i64 = tx.query_row(
            "UPDATE _tables SET next_id = next_id + 1 WHERE name = ?1 RETURNING next_id - 1",
            [table],
            |row| row.get(0),
        )?;
        let at = self.timestamp()?;
        let doc = Doc {
            id,
            version: 1,
            created_at: at.clone(),
            updated_at: at.clone(),
            fields,
        };
        tx.execute(
            "INSERT INTO docs (tbl, id, version, created_at, updated_at, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![table, doc.id, doc.version, at, at, to_json(&doc.fields)?],
        )?;
        let change = record(&tx, ChangeKind::Insert, table, at, Some(doc.clone()))?;
        tx.commit()?;
        self.publish(&change);
        Ok(doc)
    }

    /// Reads one document by id.
    ///
    /// # Errors
    /// [`DbError::NotFound`] when no such document exists.
    pub fn get(&self, table: &str, id: i64) -> Result<Doc> {
        let conn = self.lock()?;
        load_def(&conn, table)?;
        read_doc(&conn, table, id)
    }

    /// Changes the fields named in `patch` and leaves the rest alone. A `null`
    /// value removes an optional field. Pass the `version` you last read as
    /// `expected_version` to refuse the write if someone else got there first.
    ///
    /// # Errors
    /// Fails on a schema mismatch, a missing document, or a version conflict.
    pub fn update(
        &self,
        table: &str,
        id: i64,
        patch: Value,
        expected_version: Option<i64>,
    ) -> Result<Doc> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let def = load_def(&tx, table)?;
        let current = read_doc(&tx, table, id)?;
        if let Some(expected) = expected_version
            && expected != current.version
        {
            return Err(DbError::VersionConflict {
                table: table.to_owned(),
                id,
                expected,
                actual: current.version,
            });
        }
        let mut fields = current.fields;
        for (name, value) in def.check(into_object(patch)?)? {
            if value.is_null() {
                fields.remove(&name);
            } else {
                fields.insert(name, value);
            }
        }
        def.check_required(&fields)?;
        check_references(&tx, &def, &fields)?;
        let at = self.timestamp()?;
        let doc = Doc {
            id,
            version: current.version + 1,
            created_at: current.created_at,
            updated_at: at.clone(),
            fields,
        };
        tx.execute(
            "UPDATE docs SET version = ?3, updated_at = ?4, body = ?5 WHERE tbl = ?1 AND id = ?2",
            params![table, id, doc.version, at, to_json(&doc.fields)?],
        )?;
        let change = record(&tx, ChangeKind::Update, table, at, Some(doc.clone()))?;
        tx.commit()?;
        self.publish(&change);
        Ok(doc)
    }

    /// Removes a document.
    ///
    /// # Errors
    /// Fails when the document is missing or other documents still point to it.
    pub fn delete(&self, table: &str, id: i64) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        load_def(&tx, table)?;
        let doc = read_doc(&tx, table, id)?;
        check_not_referenced(&tx, table, id)?;
        tx.execute(
            "DELETE FROM docs WHERE tbl = ?1 AND id = ?2",
            params![table, id],
        )?;
        let change = record(&tx, ChangeKind::Delete, table, self.timestamp()?, Some(doc))?;
        tx.commit()?;
        self.publish(&change);
        Ok(())
    }

    /// Returns the documents matching `query`, one page at a time.
    ///
    /// # Errors
    /// Fails when the query names an unknown table or field, or uses an
    /// operator or value the field's type does not accept.
    pub fn find(&self, query: &Query) -> Result<Page> {
        let conn = self.lock()?;
        let def = load_def(&conn, &query.table)?;
        let compiled = query.compile(&def)?;
        let mut params = vec![SqlValue::Text(query.table.clone())];
        params.extend(compiled.params);
        let total: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM docs WHERE tbl = ?{}",
                compiled.conditions
            ),
            params_from_iter(params.iter()),
            |row| row.get(0),
        )?;
        params.push(SqlValue::Integer(i64::from(query.effective_limit())));
        params.push(SqlValue::Integer(i64::from(query.offset)));
        let sql = format!(
            "SELECT {DOC_COLUMNS} FROM docs WHERE tbl = ?{} ORDER BY {} LIMIT ? OFFSET ?",
            compiled.conditions, compiled.order
        );
        let docs = conn
            .prepare(&sql)?
            .query_map(params_from_iter(params.iter()), raw_doc)?
            .map(|raw| raw.map_err(DbError::from).and_then(RawDoc::into_doc))
            .collect::<Result<Vec<Doc>>>()?;
        let end = query
            .offset
            .saturating_add(u32::try_from(docs.len()).unwrap_or(u32::MAX));
        Ok(Page {
            docs,
            total,
            next_offset: (i64::from(end) < total).then_some(end),
        })
    }

    /// Returns up to 500 writes with a `seq` greater than `seq`, oldest first.
    /// Start from 0 and pass the last `seq` you saw to catch up.
    ///
    /// # Errors
    /// Fails only on a storage error.
    pub fn changes_since(&self, seq: i64) -> Result<Vec<Change>> {
        let conn = self.lock()?;
        conn.prepare(
            "SELECT seq, tbl, kind, at, doc FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?
        .query_map(params![seq, MAX_CHANGES], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .map(|row| {
            let (seq, table, kind, at, doc) = row?;
            Ok(Change {
                seq,
                table,
                kind: ChangeKind::parse(&kind)?,
                at,
                doc: from_json(&doc)?,
            })
        })
        .collect()
    }

    /// Returns a channel that receives every write from now on, in order.
    /// Drop the receiver to unsubscribe.
    ///
    /// # Errors
    /// Fails only if the database was left unusable by a crashed thread.
    pub fn subscribe(&self) -> Result<Receiver<Change>> {
        let (sender, receiver) = mpsc::channel();
        self.subscribers
            .lock()
            .map_err(|error| DbError::Internal(error.to_string()))?
            .push(sender);
        Ok(receiver)
    }

    /// Called while the connection lock is still held, so subscribers see
    /// changes in `seq` order.
    pub(crate) fn publish(&self, change: &Change) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(change.clone()).is_ok());
        }
    }
}

struct RawDoc {
    id: i64,
    version: i64,
    created_at: String,
    updated_at: String,
    body: String,
}

impl RawDoc {
    fn into_doc(self) -> Result<Doc> {
        Ok(Doc {
            id: self.id,
            version: self.version,
            created_at: self.created_at,
            updated_at: self.updated_at,
            fields: from_json(&self.body)?,
        })
    }
}

fn raw_doc(row: &Row<'_>) -> rusqlite::Result<RawDoc> {
    Ok(RawDoc {
        id: row.get(0)?,
        version: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        body: row.get(4)?,
    })
}

fn read_doc(conn: &Connection, table: &str, id: i64) -> Result<Doc> {
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM docs WHERE tbl = ?1 AND id = ?2"),
        params![table, id],
        raw_doc,
    )
    .optional()?
    .ok_or_else(|| DbError::NotFound {
        table: table.to_owned(),
        id,
    })?
    .into_doc()
}

pub(crate) fn doc_exists(conn: &Connection, table: &str, id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM docs WHERE tbl = ?1 AND id = ?2)",
        params![table, id],
        |row| row.get(0),
    )?)
}

pub(crate) fn count_docs(conn: &Connection, table: &str) -> Result<i64> {
    Ok(
        conn.query_row("SELECT count(*) FROM docs WHERE tbl = ?1", [table], |row| {
            row.get(0)
        })?,
    )
}

pub(crate) fn all_defs(conn: &Connection) -> Result<Vec<TableDef>> {
    conn.prepare("SELECT schema FROM _tables ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|schema| from_json(&schema?))
        .collect()
}

pub(crate) fn load_def(conn: &Connection, table: &str) -> Result<TableDef> {
    let schema: Option<String> = conn
        .query_row(
            "SELECT schema FROM _tables WHERE name = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(schema) = schema {
        return from_json(&schema);
    }
    let available: Vec<String> = all_defs(conn)?.into_iter().map(|def| def.name).collect();
    Err(DbError::UnknownTable {
        table: table.to_owned(),
        suggestion: closest(table, &available),
        available,
    })
}

/// A reference field must point to an existing table, or to the table being
/// defined (`own_table`).
pub(crate) fn check_ref_target(
    conn: &Connection,
    kind: &FieldType,
    own_table: Option<&str>,
) -> Result<()> {
    if let FieldType::Ref { table } = kind
        && own_table != Some(table.as_str())
    {
        load_def(conn, table)?;
    }
    Ok(())
}

fn check_references(conn: &Connection, def: &TableDef, fields: &Map<String, Value>) -> Result<()> {
    for (field, target, id) in def.references(fields) {
        if !doc_exists(conn, target, id)? {
            return Err(DbError::BrokenReference {
                field: field.to_owned(),
                target: target.to_owned(),
                id,
            });
        }
    }
    Ok(())
}

fn check_not_referenced(conn: &Connection, table: &str, id: i64) -> Result<()> {
    for def in all_defs(conn)? {
        for field in &def.fields {
            if field.kind
                != (FieldType::Ref {
                    table: table.to_owned(),
                })
            {
                continue;
            }
            let count: i64 = conn.query_row(
                &format!(
                    "SELECT count(*) FROM docs WHERE tbl = ?1 AND json_extract(body, '$.{}') = ?2",
                    field.name
                ),
                params![def.name, id],
                |row| row.get(0),
            )?;
            if count > 0 {
                return Err(DbError::StillReferenced {
                    table: table.to_owned(),
                    id,
                    by_table: def.name,
                    by_field: field.name.clone(),
                    count,
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn record(
    conn: &Connection,
    kind: ChangeKind,
    table: &str,
    at: String,
    doc: Option<Doc>,
) -> Result<Change> {
    conn.execute(
        "INSERT INTO changes (tbl, kind, at, doc) VALUES (?1, ?2, ?3, ?4)",
        params![table, kind.as_str(), at, to_json(&doc)?],
    )?;
    Ok(Change {
        seq: conn.last_insert_rowid(),
        table: table.to_owned(),
        kind,
        at,
        doc,
    })
}

fn into_object(value: Value) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(DbError::NotAnObject {
            got: other.to_string(),
        }),
    }
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| DbError::Internal(error.to_string()))
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|error| DbError::Internal(error.to_string()))
}
