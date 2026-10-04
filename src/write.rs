//! Writes to documents. One write or a batch of them runs as a single
//! transaction: every write takes effect, or none does.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::change::{Change, ChangeKind};
use crate::db::{AgentDb, Doc, all_defs, doc_exists, load_def, read_doc, record, to_json};
use crate::error::{DbError, Result};
use crate::index;
use crate::query::sql_value;
use crate::schema::TableDef;

/// The most writes one [`AgentDb::batch`] call accepts.
pub(crate) const MAX_BATCH: usize = 500;

/// One write to a document. Pass several to [`AgentDb::batch`] to apply
/// them as a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Write {
    Insert {
        table: String,
        doc: Value,
    },
    /// Changes the fields named in `patch`; a `null` value removes an
    /// optional field. With `version`, the write is refused unless the
    /// document is still at that version.
    Update {
        table: String,
        id: i64,
        patch: Value,
        #[serde(default)]
        version: Option<i64>,
    },
    /// With `version`, the write is refused unless the document is still at
    /// that version.
    Delete {
        table: String,
        id: i64,
        #[serde(default)]
        version: Option<i64>,
    },
}

impl AgentDb {
    /// Applies up to 500 writes in order as one unit: if any of them fails,
    /// none takes effect. Returns one document per write, in order: the
    /// document after the write, or its last state for a delete. A later
    /// write in the batch sees the earlier ones.
    ///
    /// # Errors
    /// [`DbError::BatchTooLarge`] for more than 500 writes. Otherwise the
    /// failing write's error; with more than one write it is wrapped in
    /// [`DbError::StepFailed`], which says which write it was.
    pub fn batch(&self, writes: Vec<Write>) -> Result<Vec<Doc>> {
        let of = writes.len();
        if of > MAX_BATCH {
            return Err(DbError::BatchTooLarge {
                size: of,
                max: MAX_BATCH,
            });
        }
        let changes = self.write(|conn, at| {
            writes
                .into_iter()
                .enumerate()
                .map(|(index, write)| {
                    apply(conn, write, at).map_err(|source| source.at_step(index + 1, of))
                })
                .collect()
        })?;
        Ok(changes
            .into_iter()
            .filter_map(|change| change.doc)
            .collect())
    }

    /// Stores a new document and returns it with its assigned `id`.
    ///
    /// # Errors
    /// Fails when the document does not match the table's schema or points to
    /// a document that does not exist.
    pub fn insert(&self, table: &str, doc: Value) -> Result<Doc> {
        self.write_one(Write::Insert {
            table: table.to_owned(),
            doc,
        })
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
        self.write_one(Write::Update {
            table: table.to_owned(),
            id,
            patch,
            version: expected_version,
        })
    }

    /// Removes a document. Pass the `version` you last read as
    /// `expected_version` to refuse the delete if someone else changed the
    /// document since.
    ///
    /// # Errors
    /// Fails when the document is missing, other documents still point to
    /// it, or on a version conflict.
    pub fn delete(&self, table: &str, id: i64, expected_version: Option<i64>) -> Result<()> {
        self.write_one(Write::Delete {
            table: table.to_owned(),
            id,
            version: expected_version,
        })?;
        Ok(())
    }

    fn write_one(&self, write: Write) -> Result<Doc> {
        self.batch(vec![write])?
            .pop()
            .ok_or_else(|| DbError::Internal("a write returned no document".to_owned()))
    }
}

fn apply(conn: &Connection, write: Write, at: &str) -> Result<Change> {
    match write {
        Write::Insert { table, doc } => insert(conn, &table, doc, at),
        Write::Update {
            table,
            id,
            patch,
            version,
        } => update(conn, &table, id, version, patch, at),
        Write::Delete { table, id, version } => delete(conn, &table, id, version, at),
    }
}

fn insert(conn: &Connection, table: &str, doc: Value, at: &str) -> Result<Change> {
    let def = load_def(conn, table)?;
    let mut fields = def.check(into_object(doc)?)?;
    fields.retain(|_, value| !value.is_null());
    def.check_required(&fields)?;
    check_references(conn, &def, &fields)?;
    let id: i64 = conn.query_row(
        "INSERT INTO _ids (name, next_id) VALUES (?1, 2)
         ON CONFLICT (name) DO UPDATE SET next_id = next_id + 1
         RETURNING next_id - 1",
        [table],
        |row| row.get(0),
    )?;
    check_unique(conn, &def, &fields, id)?;
    add_to_count(conn, table, 1)?;
    let doc = Doc {
        id,
        version: 1,
        created_at: at.to_owned(),
        updated_at: at.to_owned(),
        fields,
    };
    conn.execute(
        "INSERT INTO docs (tbl, id, version, created_at, updated_at, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![table, doc.id, doc.version, at, at, to_json(&doc.fields)?],
    )?;
    record(conn, ChangeKind::Insert, table, at, Some(doc))
}

fn update(
    conn: &Connection,
    table: &str,
    id: i64,
    expected_version: Option<i64>,
    patch: Value,
    at: &str,
) -> Result<Change> {
    let def = load_def(conn, table)?;
    let current = read_doc(conn, table, id)?;
    check_version(table, &current, expected_version)?;
    let mut fields = current.fields;
    for (name, value) in def.check(into_object(patch)?)? {
        if value.is_null() {
            fields.remove(&name);
        } else {
            fields.insert(name, value);
        }
    }
    def.check_required(&fields)?;
    check_references(conn, &def, &fields)?;
    check_unique(conn, &def, &fields, id)?;
    let doc = Doc {
        id,
        version: current.version + 1,
        created_at: current.created_at,
        updated_at: at.to_owned(),
        fields,
    };
    conn.execute(
        "UPDATE docs SET version = ?3, updated_at = ?4, body = ?5 WHERE tbl = ?1 AND id = ?2",
        params![table, id, doc.version, at, to_json(&doc.fields)?],
    )?;
    record(conn, ChangeKind::Update, table, at, Some(doc))
}

fn delete(
    conn: &Connection,
    table: &str,
    id: i64,
    expected_version: Option<i64>,
    at: &str,
) -> Result<Change> {
    load_def(conn, table)?;
    let doc = read_doc(conn, table, id)?;
    check_version(table, &doc, expected_version)?;
    check_not_referenced(conn, table, id)?;
    conn.execute(
        "DELETE FROM docs WHERE tbl = ?1 AND id = ?2",
        params![table, id],
    )?;
    add_to_count(conn, table, -1)?;
    record(conn, ChangeKind::Delete, table, at, Some(doc))
}

fn check_version(table: &str, current: &Doc, expected_version: Option<i64>) -> Result<()> {
    match expected_version {
        Some(expected) if expected != current.version => Err(DbError::VersionConflict {
            table: table.to_owned(),
            id: current.id,
            expected,
            actual: current.version,
        }),
        _ => Ok(()),
    }
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

/// Refuses a value that another document already holds in a unique field.
fn check_unique(
    conn: &Connection,
    def: &TableDef,
    fields: &Map<String, Value>,
    id: i64,
) -> Result<()> {
    for field in def.fields.iter().filter(|field| field.unique) {
        let Some(value) = fields.get(&field.name) else {
            continue;
        };
        let holder: Option<i64> = conn
            .query_row(
                &index::other_holder_sql(def, field),
                params![sql_value(value)?, id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(holder) = holder {
            return Err(DbError::DuplicateValue {
                table: def.name.clone(),
                field: field.name.clone(),
                value: value.to_string(),
                id: holder,
            });
        }
    }
    Ok(())
}

fn check_not_referenced(conn: &Connection, table: &str, id: i64) -> Result<()> {
    for def in all_defs(conn)? {
        for field in def.fields.iter().filter(|field| field.links_to(table)) {
            let count: i64 =
                conn.query_row(&index::count_holders_sql(&def, field), [id], |row| {
                    row.get(0)
                })?;
            if count > 0 {
                return Err(DbError::StillReferenced {
                    table: table.to_owned(),
                    id,
                    by_table: def.name.clone(),
                    by_field: field.name.clone(),
                    count,
                });
            }
        }
    }
    Ok(())
}

fn add_to_count(conn: &Connection, table: &str, change: i64) -> Result<()> {
    conn.execute(
        "UPDATE _tables SET count = count + ?2 WHERE name = ?1",
        params![table, change],
    )?;
    Ok(())
}

fn into_object(value: Value) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(DbError::NotAnObject {
            got: other.to_string(),
        }),
    }
}
