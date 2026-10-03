use serde::Serialize;

use crate::db::Doc;
use crate::error::{DbError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Insert,
    Update,
    Delete,
    /// The table's fields or name changed. Call `describe()` to see the new shape.
    Schema,
}

impl ChangeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Schema => "schema",
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Self> {
        match text {
            "insert" => Ok(Self::Insert),
            "update" => Ok(Self::Update),
            "delete" => Ok(Self::Delete),
            "schema" => Ok(Self::Schema),
            other => Err(DbError::Internal(format!("unknown change kind `{other}`"))),
        }
    }
}

/// One committed write. `seq` grows by one per write, so a reader can ask for
/// everything after the last `seq` it saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub seq: i64,
    pub table: String,
    pub kind: ChangeKind,
    pub at: String,
    /// The document after the write, or its last state for a delete. `None`
    /// for a schema change.
    pub doc: Option<Doc>,
}
