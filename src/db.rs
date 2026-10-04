use std::ffi::c_int;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::types::Value as SqlValue;
use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Row, TransactionBehavior, params, params_from_iter,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;

use crate::ask::{self, Asked, Plan};
use crate::change::{Change, ChangeKind};
use crate::error::{DbError, Result};
use crate::format::{prepare, supported_version};
use crate::jev::Judge;
use crate::query::{Compiled, Query, fields_with_index, register_functions};
use crate::schema::{Field, FieldType, TableDef, closest, format_utc};

pub(crate) const DOC_COLUMNS: &str = "id, version, created_at, updated_at, body";
const CHANGES_PER_CALL: i64 = 500;
/// The change log keeps this many of the newest changes and drops the rest.
pub(crate) const RETAINED_CHANGES: i64 = 10_000;
/// How long a write waits for another process that is writing to the same file.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// How many changes a subscriber may fall behind before it is told it lagged.
const SUBSCRIBER_BUFFER: usize = 1024;
/// How long one read may run before it is stopped. A tenant's reads run one
/// at a time, so a read that takes this long holds up every other agent.
const READ_BUDGET: Duration = Duration::from_secs(2);
/// How many steps SQLite takes between two looks at the clock.
const STEPS_PER_CLOCK_CHECK: c_int = 1_000;

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
    writer: Mutex<Connection>,
    /// A second connection to the same file, so a read never waits for a
    /// write in progress. A database in memory has only the writer.
    reader: Option<Mutex<Connection>>,
    live: broadcast::Sender<Change>,
    frozen_time: Mutex<Option<OffsetDateTime>>,
    read_budget: Duration,
}

impl AgentDb {
    /// Opens the database file at `path`, creating it if needed. The whole
    /// file is encrypted with `key`.
    ///
    /// # Errors
    /// [`DbError::EmptyKey`] for an empty key, [`DbError::WrongKey`] when the key
    /// does not match an existing file, [`DbError::NewerFormat`] for a file
    /// written by a newer agentdb, which is left exactly as it was.
    pub fn open(path: impl AsRef<Path>, key: &str) -> Result<Self> {
        if key.is_empty() {
            return Err(DbError::EmptyKey);
        }
        let mut writer = connect(path.as_ref(), key)?;
        supported_version(&writer)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        writer.pragma_update(None, "synchronous", "FULL")?;
        prepare(&mut writer)?;
        let reader = connect(path.as_ref(), key)?;
        reader.pragma_update(None, "query_only", true)?;
        Ok(Self::new(writer, Some(reader)))
    }

    /// Opens an unencrypted database that lives only in memory.
    ///
    /// # Errors
    /// Fails only if the storage engine cannot start.
    pub fn open_in_memory() -> Result<Self> {
        let mut writer = Connection::open_in_memory()?;
        register_functions(&writer)?;
        prepare(&mut writer)?;
        Ok(Self::new(writer, None))
    }

    fn new(writer: Connection, reader: Option<Connection>) -> Self {
        Self {
            writer: Mutex::new(writer),
            reader: reader.map(Mutex::new),
            live: broadcast::Sender::new(SUBSCRIBER_BUFFER),
            frozen_time: Mutex::new(None),
            read_budget: READ_BUDGET,
        }
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

    fn timestamp(&self) -> Result<String> {
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
        let defs = self.read(all_defs)?;
        let plan = ask::plan(&defs, text, self.now()?.date(), judge)?;
        self.answer(plan)
    }

    /// Runs the query of a plan that was not refused. A query the table is
    /// too large to answer becomes a refusal: the request was understood,
    /// and the caller gets the query along with the reason.
    fn answer(&self, plan: Plan) -> Result<Asked> {
        let (page, refusal) = match (&plan.query, plan.refusal) {
            (Some(query), None) => match self.find(query) {
                Ok(page) => (Some(page), None),
                Err(error @ (DbError::QueryNeedsIndex { .. } | DbError::QueryTooSlow { .. })) => {
                    (None, Some(error.to_string()))
                }
                Err(error) => return Err(error),
            },
            (_, refusal) => (None, refusal),
        };
        Ok(Asked {
            query: plan.query,
            confidence: plan.confidence,
            refusal,
            page,
            usage: plan.usage,
        })
    }

    /// Runs `work` against one snapshot of the database, so everything it
    /// reads belongs to the same moment: a write is seen whole or not at all.
    /// A statement still running when the read budget is spent is stopped.
    pub(crate) fn read<T>(&self, work: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let mut conn = lock(self.reader.as_ref().unwrap_or(&self.writer))?;
        let snapshot = conn.transaction()?;
        let found = within(self.read_budget, &snapshot, work)?;
        snapshot.finish()?;
        Ok(found)
    }

    /// Runs `work` in one write transaction, handing it the time of the
    /// write. The changes it returns reach subscribers once they are stored,
    /// in `seq` order, because the writer stays locked until they are sent.
    pub(crate) fn write(
        &self,
        work: impl FnOnce(&Connection, &str) -> Result<Vec<Change>>,
    ) -> Result<Vec<Change>> {
        let mut conn = lock(&self.writer)?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changes = work(&transaction, &self.timestamp()?)?;
        trim_change_log(&transaction)?;
        transaction.commit()?;
        for change in &changes {
            let nobody_is_subscribed = self.live.send(change.clone()).is_err();
            if nobody_is_subscribed {
                break;
            }
        }
        Ok(changes)
    }

    /// Lists every table with its fields and document count. An agent's first
    /// call: it shows everything needed to read and write.
    ///
    /// # Errors
    /// Fails only on a storage error.
    pub fn describe(&self) -> Result<Vec<TableInfo>> {
        self.read(|conn| {
            all_defs(conn)?
                .into_iter()
                .map(|def| {
                    Ok(TableInfo {
                        count: count_docs(conn, &def.name)?,
                        name: def.name,
                        description: def.description,
                        fields: def.fields,
                    })
                })
                .collect()
        })
    }

    /// Reads one document by id.
    ///
    /// # Errors
    /// [`DbError::NotFound`] when no such document exists.
    pub fn get(&self, table: &str, id: i64) -> Result<Doc> {
        self.read(|conn| {
            load_def(conn, table)?;
            read_doc(conn, table, id)
        })
    }

    /// Returns the documents matching `query`, one page at a time. Skipping
    /// to a far `offset` costs time in proportion to the offset.
    ///
    /// # Errors
    /// Fails when the query names an unknown table or field, or uses an
    /// operator or value the field's type does not accept.
    /// [`DbError::QueryNeedsIndex`] when the table holds more than 1,000
    /// documents and no index can answer the query.
    /// [`DbError::QueryTooSlow`] when the query was stopped because it ran
    /// for longer than one read may take.
    pub fn find(&self, query: &Query) -> Result<Page> {
        self.read(|conn| {
            let def = load_def(conn, &query.table)?;
            let table_size = count_docs(conn, &def.name)?;
            let compiled = query.compile(&def, table_size)?;
            matching_page(conn, query, compiled, table_size)
                .map_err(|error| self.explain_stop(error, &def, table_size))
        })
    }

    /// Turns the storage error of a query that ran out of read budget into
    /// one that says what happened and which queries are fast.
    fn explain_stop(&self, error: DbError, def: &TableDef, table_size: i64) -> DbError {
        match error {
            DbError::Storage(stopped)
                if stopped.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) =>
            {
                DbError::QueryTooSlow {
                    table: def.name.clone(),
                    count: table_size,
                    budget: self.read_budget,
                    indexed: fields_with_index(def),
                }
            }
            other => other,
        }
    }

    /// Returns up to 500 writes with a `seq` greater than `seq`, oldest first.
    /// Start from 0 and pass the last `seq` you saw to catch up; a negative
    /// `seq` means 0. Only the newest 10,000 changes are kept.
    ///
    /// # Errors
    /// [`DbError::ChangesTrimmed`] when changes after `seq` have already been
    /// dropped from the log, so the caller cannot catch up from there.
    /// [`DbError::SinceAhead`] when the log has not reached `seq` yet:
    /// waiting for it would hide every write made until then.
    pub fn changes_since(&self, seq: i64) -> Result<Vec<Change>> {
        let seq = seq.max(0);
        self.read(|conn| {
            let (oldest, latest) = change_log_range(conn)?;
            if seq < oldest - 1 {
                return Err(DbError::ChangesTrimmed {
                    since: seq,
                    oldest,
                    latest,
                });
            }
            if seq > latest {
                return Err(DbError::SinceAhead { since: seq, latest });
            }
            conn.prepare(
                "SELECT seq, tbl, kind, at, doc FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?
            .query_map(params![seq, CHANGES_PER_CALL], |row| {
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
        })
    }

    /// The `seq` of the newest write, or 0 when nothing was ever written.
    /// A caller that just read the current state resumes from here.
    ///
    /// # Errors
    /// Fails only on a storage error.
    pub fn latest_seq(&self) -> Result<i64> {
        self.read(|conn| Ok(change_log_range(conn)?.1))
    }

    /// Returns a receiver for every write from now on, in `seq` order. Drop
    /// it to unsubscribe. A receiver that falls more than 1,024 changes
    /// behind gets `Lagged` and catches up with [`AgentDb::changes_since`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.live.subscribe()
    }
}

fn connect(path: &Path, key: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "key", key)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // The key is only tested when the file is first read.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|error| match error.sqlite_error_code() {
        Some(ErrorCode::NotADatabase) => DbError::WrongKey,
        _ => DbError::Storage(error),
    })?;
    register_functions(&conn)?;
    Ok(conn)
}

fn lock(conn: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>> {
    conn.lock()
        .map_err(|error| DbError::Internal(error.to_string()))
}

/// Runs `work` on `conn`, stopping the statement it is running once
/// `budget` has passed. The connection is left without a time limit.
fn within<T>(
    budget: Duration,
    conn: &Connection,
    work: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    conn.progress_handler(
        STEPS_PER_CLOCK_CHECK,
        Some(move || started.elapsed() >= budget),
    )?;
    let outcome = work(conn);
    conn.progress_handler(0, None::<fn() -> bool>)?;
    outcome
}

/// The page of `query` and how many documents match it in total.
fn matching_page(
    conn: &Connection,
    query: &Query,
    compiled: Compiled,
    table_size: i64,
) -> Result<Page> {
    let total = if query.filters.is_empty() {
        table_size
    } else {
        conn.query_row(
            &compiled.count_sql(),
            params_from_iter(compiled.params.iter()),
            |row| row.get(0),
        )?
    };
    let page_sql = compiled.page_sql();
    let mut params = compiled.params;
    params.push(SqlValue::Integer(i64::from(query.effective_limit())));
    params.push(SqlValue::Integer(i64::from(query.offset)));
    let docs = conn
        .prepare(&page_sql)?
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

/// The `seq` of the oldest change still kept and of the newest one. An
/// empty log reports `(1, 0)`.
fn change_log_range(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT coalesce((SELECT min(seq) FROM changes), 1),
                coalesce((SELECT max(seq) FROM changes), 0)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn trim_change_log(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM changes WHERE seq <= (SELECT max(seq) FROM changes) - ?1",
        [RETAINED_CHANGES],
    )?;
    Ok(())
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

pub(crate) fn read_doc(conn: &Connection, table: &str, id: i64) -> Result<Doc> {
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
    Ok(conn.query_row(
        "SELECT count FROM _tables WHERE name = ?1",
        [table],
        |row| row.get(0),
    )?)
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
    match schema {
        Some(schema) => from_json(&schema),
        None => Err(unknown_table(table, &all_defs(conn)?)),
    }
}

pub(crate) fn unknown_table(table: &str, defs: &[TableDef]) -> DbError {
    let available: Vec<String> = defs.iter().map(|def| def.name.clone()).collect();
    DbError::UnknownTable {
        table: table.to_owned(),
        suggestion: closest(table, &available),
        available,
    }
}

pub(crate) fn store_def(conn: &Connection, def: &TableDef) -> Result<()> {
    conn.execute(
        "UPDATE _tables SET schema = ?2 WHERE name = ?1",
        params![def.name, to_json(def)?],
    )?;
    Ok(())
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

pub(crate) fn record(
    conn: &Connection,
    kind: ChangeKind,
    table: &str,
    at: &str,
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
        at: at.to_owned(),
        doc,
    })
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| DbError::Internal(error.to_string()))
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|error| DbError::Internal(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;
    use std::time::Duration;

    use serde_json::json;

    use super::AgentDb;
    use crate::ask::Plan;
    use crate::jev::Usage;
    use crate::query::MAX_SCAN_DOCS;
    use crate::{DbError, FieldType, Op, Query, TableDef, Write};

    fn notes_db(count: i64) -> AgentDb {
        let db = AgentDb::open_in_memory().unwrap();
        add_notes(&db, count);
        db
    }

    fn add_notes(db: &AgentDb, count: i64) {
        db.define_table(&TableDef::new("notes").required("text", FieldType::Text))
            .unwrap();
        let writes = (0..count).map(|_| Write::Insert {
            table: "notes".to_owned(),
            doc: json!({"text": "hello"}),
        });
        for batch in writes.collect::<Vec<_>>().chunks(500) {
            db.batch(batch.to_vec()).unwrap();
        }
    }

    /// A budget of zero stops every read that SQLite takes more than a
    /// thousand steps over, however fast the machine is.
    fn with_read_budget(mut db: AgentDb, budget: Duration) -> AgentDb {
        db.read_budget = budget;
        db
    }

    fn reads_every_note() -> Query {
        Query::table("notes").filter("text", Op::Contains, "zzz")
    }

    fn reads_one_note() -> Query {
        Query::table("notes").filter("id", Op::Eq, 1)
    }

    #[test]
    fn a_read_that_runs_out_of_budget_is_stopped_and_told_what_is_fast() {
        let db = with_read_budget(notes_db(500), Duration::ZERO);
        let stopped = db.find(&reads_every_note()).unwrap_err();
        assert_eq!(stopped.code(), "query_too_slow");
        assert_eq!(
            stopped.to_string(),
            "this query was stopped after 0ns, the longest one read may run: it had to read too many of the 500 documents in `notes`. Fast on a table this size: an `eq` filter on an indexed field that matches few documents, a narrow `gt`, `gte`, `lt` or `lte` range on an indexed field, a small `offset`. Slow: a range that covers most of the table, `contains` or a sort over many documents, a large `offset`. Indexed fields: id, created_at, updated_at. Narrow the query and send it again."
        );
        assert_eq!(db.find(&reads_one_note()).unwrap().total, 1);
        assert_eq!(notes_db(500).find(&reads_every_note()).unwrap().total, 0);
    }

    #[test]
    fn a_stopped_read_does_not_limit_the_writes_that_share_its_connection() {
        let db = with_read_budget(notes_db(500), Duration::ZERO);
        db.find(&reads_every_note()).unwrap_err();
        let writes = vec![
            Write::Insert {
                table: "notes".to_owned(),
                doc: json!({"text": "hello"}),
            };
            500
        ];
        assert_eq!(db.batch(writes).unwrap().len(), 500);
        db.rename_field("notes", "text", "body").unwrap();
        db.change_field_type(
            "notes",
            "body",
            FieldType::Enum {
                values: vec!["hello".to_owned()],
            },
        )
        .unwrap();
        assert_eq!(db.get("notes", 1000).unwrap().fields["body"], "hello");
    }

    #[test]
    fn a_stopped_read_leaves_the_read_connection_of_a_file_usable() {
        let dir = tempfile::tempdir().unwrap();
        let opened = AgentDb::open(dir.path().join("tenant.db"), "key").unwrap();
        let db = with_read_budget(opened, Duration::ZERO);
        add_notes(&db, 500);
        for _ in 0..2 {
            assert!(matches!(
                db.find(&reads_every_note()),
                Err(DbError::QueryTooSlow { count: 500, .. })
            ));
            assert_eq!(db.find(&reads_one_note()).unwrap().total, 1);
            assert_eq!(db.get("notes", 500).unwrap().id, 500);
            assert_eq!(db.describe().unwrap()[0].count, 500);
        }
    }

    #[test]
    fn every_read_starts_with_its_whole_budget() {
        let budget = Duration::from_millis(400);
        let db = with_read_budget(notes_db(500), budget);
        db.find(&reads_every_note()).unwrap();
        sleep(budget);
        assert_eq!(db.find(&reads_every_note()).unwrap().total, 0);
    }

    fn understood(query: Query) -> Plan {
        Plan {
            query: Some(query),
            confidence: 0.9,
            refusal: None,
            usage: Usage::default(),
        }
    }

    #[test]
    fn ask_refuses_a_request_that_no_index_can_answer() {
        let db = notes_db(MAX_SCAN_DOCS + 1);
        let query = Query::table("notes").filter("text", Op::Eq, "hello");
        let asked = db.answer(understood(query.clone())).unwrap();
        assert_eq!(asked.query, Some(query.clone()));
        assert_eq!(asked.page, None);
        assert_eq!(
            asked.refusal,
            db.find(&query).err().map(|error| error.to_string())
        );
        assert!(matches!(
            db.find(&query),
            Err(DbError::QueryNeedsIndex { .. })
        ));
    }

    #[test]
    fn ask_refuses_a_request_that_ran_out_of_budget() {
        let db = with_read_budget(notes_db(500), Duration::ZERO);
        let asked = db.answer(understood(reads_every_note())).unwrap();
        assert_eq!(asked.query, Some(reads_every_note()));
        assert_eq!(asked.page, None);
        let stopped = db.find(&reads_every_note()).unwrap_err();
        assert!(matches!(stopped, DbError::QueryTooSlow { .. }));
        assert_eq!(asked.refusal, Some(stopped.to_string()));
    }

    #[test]
    fn ask_runs_a_request_that_a_small_table_can_answer() {
        let db = notes_db(3);
        let query = Query::table("notes").filter("text", Op::Eq, "hello");
        let asked = db.answer(understood(query)).unwrap();
        assert_eq!(asked.refusal, None);
        assert_eq!(asked.page.map(|page| page.total), Some(3));
    }

    #[test]
    fn ask_still_fails_on_errors_the_caller_must_see() {
        let db = notes_db(0);
        let missing = Query::table("notes").filter("title", Op::Eq, "hello");
        assert!(matches!(
            db.answer(understood(missing)),
            Err(DbError::UnknownField { .. })
        ));
    }
}
