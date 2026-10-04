//! The storage tables, and how a file written by an older agentdb is brought
//! up to date when it is opened.

use rusqlite::{Connection, TransactionBehavior};

use crate::db::{all_defs, store_def};
use crate::error::{DbError, Result};
use crate::index;

/// Bumped whenever the layout of the storage tables changes.
const FORMAT_VERSION: i64 = 2;

const VERSION_1: &str = "
CREATE TABLE _tables (
    name TEXT PRIMARY KEY,
    schema TEXT NOT NULL,
    next_id INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE docs (
    tbl TEXT NOT NULL,
    id INTEGER NOT NULL,
    version INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    body TEXT NOT NULL,
    PRIMARY KEY (tbl, id)
);
CREATE TABLE changes (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    tbl TEXT NOT NULL,
    kind TEXT NOT NULL,
    at TEXT NOT NULL,
    doc TEXT NOT NULL
);
";

/// Version 2 moves the id counters to `_ids`, where they outlive a dropped
/// table, keeps each table's document count in `_tables`, and adds indexes.
const VERSION_2: &str = "
CREATE TABLE _ids (
    name TEXT PRIMARY KEY,
    next_id INTEGER NOT NULL
);
INSERT INTO _ids SELECT name, next_id FROM _tables;
ALTER TABLE _tables DROP COLUMN next_id;
ALTER TABLE _tables ADD COLUMN count INTEGER NOT NULL DEFAULT 0;
UPDATE _tables SET count = (SELECT count(*) FROM docs WHERE tbl = name);
CREATE INDEX docs_created_at ON docs (tbl, created_at, id);
CREATE INDEX docs_updated_at ON docs (tbl, updated_at, id);
";

/// Creates the storage tables in a new file, or upgrades an older file to
/// the current layout. Either every step happens or the file is untouched.
pub(crate) fn prepare(conn: &mut Connection) -> Result<()> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if found > FORMAT_VERSION {
        return Err(DbError::NewerFormat {
            found,
            supported: FORMAT_VERSION,
        });
    }
    if found < 1 {
        transaction.execute_batch(VERSION_1)?;
    }
    if found < 2 {
        transaction.execute_batch(VERSION_2)?;
        index_ref_fields(&transaction)?;
    }
    transaction.pragma_update(None, "user_version", FORMAT_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn index_ref_fields(conn: &Connection) -> Result<()> {
    for mut def in all_defs(conn)? {
        def.index_unique_and_ref_fields();
        store_def(conn, &def)?;
    }
    index::sync(conn)
}
