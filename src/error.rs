use std::time::Duration;

use thiserror::Error;

use crate::db::RETAINED_CHANGES;

pub type Result<T> = std::result::Result<T, DbError>;

/// Every message names what was wrong and what a correct call looks like,
/// because the reader is an agent that will retry from the message alone.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DbError {
    #[error("unknown table `{table}`.{} Existing tables: {}.", hint(.suggestion.as_deref()), list(.available))]
    UnknownTable {
        table: String,
        suggestion: Option<String>,
        available: Vec<String>,
    },

    #[error("unknown field `{field}` on table `{table}`.{} Valid fields: {}.", hint(.suggestion.as_deref()), list(.available))]
    UnknownField {
        table: String,
        field: String,
        suggestion: Option<String>,
        available: Vec<String>,
    },

    #[error("field `{field}` on table `{table}` expects {expected}, got {got}.")]
    WrongType {
        table: String,
        field: String,
        expected: String,
        got: String,
    },

    #[error("table `{table}` requires these fields and they are missing: {}.", list(.fields))]
    MissingRequired { table: String, fields: Vec<String> },

    #[error("a document must be a JSON object like {{\"name\": \"Acme\"}}, got {got}.")]
    NotAnObject { got: String },

    #[error("`{field}` is set by the database. Remove it from the document.")]
    ReservedField { field: String },

    #[error(
        "field `{field}` points to `{target}` id {id}, which does not exist. Insert that `{target}` document first or use an existing id."
    )]
    BrokenReference {
        field: String,
        target: String,
        id: i64,
    },

    #[error("no document with id {id} in table `{table}`.")]
    NotFound { table: String, id: i64 },

    #[error(
        "`{table}` id {id} is at version {actual}, but this write expected version {expected}. Someone else changed it: read it again with get() and retry with version {actual}."
    )]
    VersionConflict {
        table: String,
        id: i64,
        expected: i64,
        actual: i64,
    },

    #[error(
        "cannot delete `{table}` id {id}: {count} document(s) in `{by_table}` point to it through `{by_field}`. Update or delete those first."
    )]
    StillReferenced {
        table: String,
        id: i64,
        by_table: String,
        by_field: String,
        count: i64,
    },

    #[error("table `{table}` already exists. Use add_field() to extend it.")]
    TableExists { table: String },

    #[error("field `{field}` already exists on table `{table}`.")]
    FieldExists { table: String, field: String },

    #[error(
        "cannot add required field `{field}` to `{table}`: it already holds {count} document(s) that would lack it. Add the field as optional."
    )]
    RequiredFieldOnExistingDocs {
        table: String,
        field: String,
        count: i64,
    },

    #[error(
        "invalid name `{name}`. Names start with a lowercase letter and use only lowercase letters, digits and underscores (max 64 characters)."
    )]
    InvalidName { name: String },

    #[error("enum field `{field}` needs at least one allowed value.")]
    EmptyEnum { field: String },

    #[error(
        "enum field `{field}` lists `{value}` more than once. List every allowed value exactly once."
    )]
    RepeatedEnumValue { field: String, value: String },

    #[error(
        "enum field `{field}` cannot allow an empty value. Give every allowed value at least one character; leave the field unset to mean \"no value\"."
    )]
    BlankEnumValue { field: String },

    #[error("operator `{op}` does not work on field `{field}` ({field_type}). Operators for this field: {}.", list(.allowed))]
    InvalidOperator {
        field: String,
        op: String,
        field_type: String,
        allowed: Vec<String>,
    },

    #[error(
        "this would permanently delete {what}, affecting {count} document(s). Call again with force = true if that is intended."
    )]
    WouldDestroy { what: String, count: i64 },

    #[error(
        "cannot drop table `{table}`: field `{by_field}` on table `{by_table}` links to it. Remove that field first."
    )]
    TableReferenced {
        table: String,
        by_table: String,
        by_field: String,
    },

    #[error(
        "cannot make `{field}` required on `{table}`: {count} document(s) have no value for it. Fill them in first; find them by filtering `{field}` eq null."
    )]
    MissingValues {
        table: String,
        field: String,
        count: i64,
    },

    #[error("field `{field}` on `{table}` is not an enum, so it has no list of allowed values.")]
    NotAnEnum { table: String, field: String },

    #[error("`{value}` is already an allowed value of `{field}` on `{table}`.")]
    EnumValueExists {
        table: String,
        field: String,
        value: String,
    },

    #[error(
        "this database file uses storage format {found}, but this version of agentdb only understands up to {supported}. Upgrade agentdb."
    )]
    NewerFormat { found: i64, supported: i64 },

    #[error(
        "cannot change `{field}` on `{table}` to {to}: {count} document(s) hold a value that does not fit, for example id {example_id} with {example}. Fix those values first; nothing was changed."
    )]
    CannotConvert {
        table: String,
        field: String,
        to: String,
        count: usize,
        example_id: i64,
        example: String,
    },

    #[error(
        "cannot remove `{value}` from `{field}` on `{table}`: {count} document(s) still use it. Update them to another value first."
    )]
    EnumValueInUse {
        table: String,
        field: String,
        value: String,
        count: i64,
    },

    #[error("`{value}` is not an allowed value of `{field}` on `{table}`. Allowed values: {}.", list(.allowed))]
    EnumValueMissing {
        table: String,
        field: String,
        value: String,
        allowed: Vec<String>,
    },

    #[error("step {step} of {of} failed, so none of the {of} changes were applied: {source}")]
    StepFailed {
        step: usize,
        of: usize,
        source: Box<Self>,
    },

    #[error(
        "`{field}` must be unique on `{table}`, and `{table}` id {id} already holds {value}. Update that document instead of adding another one, or use a different value."
    )]
    DuplicateValue {
        table: String,
        field: String,
        value: String,
        id: i64,
    },

    #[error(
        "cannot make `{field}` unique on `{table}`: {count} documents hold {value}, for example ids {first_id} and {second_id}. Change or delete all but one of them first; nothing was changed."
    )]
    DuplicatesExist {
        table: String,
        field: String,
        value: String,
        count: i64,
        first_id: i64,
        second_id: i64,
    },

    #[error(
        "cannot run this query: `{table}` holds {count} documents, and a table with more than {limit} is only searched through an index. {unserved}. Indexed fields: {}. Add a filter on one of them that leaves few documents to read (an `eq`, or a narrow `gt`, `gte`, `lt` or `lte` range){}.",
        list(.indexed),
        index_advice(.table, .could_index.as_deref())
    )]
    QueryNeedsIndex {
        table: String,
        count: i64,
        limit: i64,
        /// The filter or sort that no index can answer, as a sentence.
        unserved: String,
        indexed: Vec<String>,
        /// The field whose index would let the query run as it is.
        could_index: Option<String>,
    },

    #[error(
        "this query was stopped after {budget:?}, the longest one read may run: it had to read too many of the {count} documents in `{table}`. Fast on a table this size: an `eq` filter on an indexed field that matches few documents, a narrow `gt`, `gte`, `lt` or `lte` range on an indexed field, a small `offset`. Slow: a range that covers most of the table, `contains` or a sort over many documents, a large `offset`. Indexed fields: {}. Narrow the query and send it again.",
        list(.indexed)
    )]
    QueryTooSlow {
        table: String,
        count: i64,
        budget: Duration,
        indexed: Vec<String>,
    },

    #[error(
        "table `{table}` cannot have more than {max} indexed fields, because every index slows down each write. It would have these: {}. Ref and unique fields are always indexed. Stop indexing a field that no query filters or sorts by, using the schema change {{\"op\": \"set_indexed\", \"table\": \"{table}\", \"field\": \"<field>\", \"indexed\": false}}.",
        list(.indexed)
    )]
    TooManyIndexes {
        table: String,
        max: usize,
        indexed: Vec<String>,
    },

    #[error("`{field}` on `{table}` has to stay indexed because {reason}.")]
    IndexRequired {
        table: String,
        field: String,
        reason: &'static str,
    },

    #[error(
        "a batch takes at most {max} writes, and this one has {size}. Split it into batches of {max} or fewer and send them one after another."
    )]
    BatchTooLarge { size: usize, max: usize },

    #[error(
        "cannot replay changes after seq {since}: the log keeps only the newest {} changes, and the oldest one left is seq {oldest}. Read the current state with find() instead, then continue from seq {latest}.",
        RETAINED_CHANGES
    )]
    ChangesTrimmed {
        since: i64,
        oldest: i64,
        latest: i64,
    },

    #[error("the encryption key must not be empty.")]
    EmptyKey,

    #[error(
        "could not open the database: the encryption key is wrong, or the file is not an agentdb database."
    )]
    WrongKey,

    #[error("`{got}` is not a timestamp ({reason}). Use RFC 3339, like 2026-10-03T14:30:00Z.")]
    InvalidTimestamp { got: String, reason: String },

    #[error(
        "the TYPESAFE_API_KEY environment variable is not set, so ask() cannot reach the model. find() works without it."
    )]
    MissingApiKey,

    #[error("the language model request failed: {0}. find() works without the model.")]
    Jev(String),

    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl DbError {
    /// A short, stable name for the kind of error, for code that needs to
    /// branch on it. The message is for the agent; this is for the program.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownTable { .. } => "unknown_table",
            Self::UnknownField { .. } => "unknown_field",
            Self::WrongType { .. } => "wrong_type",
            Self::MissingRequired { .. } => "missing_required",
            Self::NotAnObject { .. } => "not_an_object",
            Self::ReservedField { .. } => "reserved_field",
            Self::BrokenReference { .. } => "broken_reference",
            Self::NotFound { .. } => "not_found",
            Self::VersionConflict { .. } => "version_conflict",
            Self::StillReferenced { .. } => "still_referenced",
            Self::TableExists { .. } => "table_exists",
            Self::FieldExists { .. } => "field_exists",
            Self::RequiredFieldOnExistingDocs { .. } => "required_field_on_existing_docs",
            Self::InvalidName { .. } => "invalid_name",
            Self::EmptyEnum { .. } => "empty_enum",
            Self::RepeatedEnumValue { .. } => "repeated_enum_value",
            Self::BlankEnumValue { .. } => "blank_enum_value",
            Self::InvalidOperator { .. } => "invalid_operator",
            Self::WouldDestroy { .. } => "would_destroy",
            Self::TableReferenced { .. } => "table_referenced",
            Self::MissingValues { .. } => "missing_values",
            Self::NotAnEnum { .. } => "not_an_enum",
            Self::EnumValueExists { .. } => "enum_value_exists",
            Self::NewerFormat { .. } => "newer_format",
            Self::CannotConvert { .. } => "cannot_convert",
            Self::EnumValueInUse { .. } => "enum_value_in_use",
            Self::EnumValueMissing { .. } => "enum_value_missing",
            Self::EmptyKey => "empty_key",
            Self::WrongKey => "wrong_key",
            Self::InvalidTimestamp { .. } => "invalid_timestamp",
            Self::MissingApiKey => "missing_api_key",
            Self::StepFailed { source, .. } => source.code(),
            Self::DuplicateValue { .. } => "duplicate_value",
            Self::DuplicatesExist { .. } => "duplicates_exist",
            Self::QueryNeedsIndex { .. } => "query_needs_index",
            Self::QueryTooSlow { .. } => "query_too_slow",
            Self::TooManyIndexes { .. } => "too_many_indexes",
            Self::IndexRequired { .. } => "index_required",
            Self::BatchTooLarge { .. } => "batch_too_large",
            Self::ChangesTrimmed { .. } => "changes_trimmed",
            Self::Jev(_) => "model_unavailable",
            Self::Storage(_) => "storage",
            Self::Internal(_) => "internal",
        }
    }

    /// Says which step of a multi-step migration or batch failed. A single
    /// step keeps its own error.
    pub(crate) fn at_step(self, step: usize, of: usize) -> Self {
        if of > 1 {
            Self::StepFailed {
                step,
                of,
                source: Box::new(self),
            }
        } else {
            self
        }
    }
}

fn hint(suggestion: Option<&str>) -> String {
    suggestion.map_or_else(String::new, |name| format!(" Did you mean `{name}`?"))
}

fn index_advice(table: &str, field: Option<&str>) -> String {
    field.map_or_else(String::new, |field| {
        format!(
            ", or index `{field}` first with the schema change {{\"op\": \"set_indexed\", \"table\": \"{table}\", \"field\": \"{field}\", \"indexed\": true}}"
        )
    })
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_owned()
    } else {
        items.join(", ")
    }
}
