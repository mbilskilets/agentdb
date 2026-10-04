use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::DOC_COLUMNS;
use crate::error::{DbError, Result};
use crate::index;
use crate::schema::{FieldType, TableDef};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
const LOWERCASE: &str = "agentdb_lowercase";
/// A table with more documents than this is only searched through an index.
pub(crate) const MAX_SCAN_DOCS: i64 = 1_000;
const INDEXED_SYSTEM_FIELDS: [&str; 3] = ["id", "created_at", "updated_at"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    /// Substring match on a text field that ignores letter case in any
    /// language.
    Contains,
}

impl Op {
    const fn name(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Contains => "contains",
        }
    }

    /// Whether an index on the field can answer a filter with this operator.
    const fn can_use_index(self) -> bool {
        !matches!(self, Self::Ne | Self::Contains)
    }

    const fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "IS NOT",
            Self::Gt => ">",
            Self::Gte => ">=",
            Self::Lt => "<",
            Self::Lte => "<=",
            Self::Contains => "contains",
        }
    }
}

const EQUALITY: &[Op] = &[Op::Eq, Op::Ne];
const ORDERED: &[Op] = &[Op::Eq, Op::Ne, Op::Gt, Op::Gte, Op::Lt, Op::Lte];
const TEXTUAL: &[Op] = &[Op::Eq, Op::Ne, Op::Contains];

const fn allowed_ops(kind: &FieldType) -> &'static [Op] {
    match kind {
        FieldType::Text => TEXTUAL,
        FieldType::Number | FieldType::Datetime => ORDERED,
        FieldType::Bool | FieldType::Enum { .. } | FieldType::Ref { .. } => EQUALITY,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub field: String,
    #[serde(default)]
    pub descending: bool,
}

/// A read of one table. All filters must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Query {
    pub table: String,
    #[serde(default, rename = "where")]
    pub filters: Vec<Filter>,
    #[serde(default)]
    pub sort: Option<Sort>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: u32,
}

impl Query {
    pub fn table(name: impl Into<String>) -> Self {
        Self {
            table: name.into(),
            filters: Vec::new(),
            sort: None,
            limit: None,
            offset: 0,
        }
    }

    #[must_use]
    pub fn filter(mut self, field: impl Into<String>, op: Op, value: impl Into<Value>) -> Self {
        self.filters.push(Filter {
            field: field.into(),
            op,
            value: value.into(),
        });
        self
    }

    #[must_use]
    pub fn sort(mut self, field: impl Into<String>, descending: bool) -> Self {
        self.sort = Some(Sort {
            field: field.into(),
            descending,
        });
        self
    }

    #[must_use]
    pub const fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    #[must_use]
    pub const fn offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    pub(crate) fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
    }

    /// Turns the query into SQL. `table_size` is how many documents the
    /// table holds: above [`MAX_SCAN_DOCS`] the query has to be one that an
    /// index can answer.
    pub(crate) fn compile(&self, def: &TableDef, table_size: i64) -> Result<Compiled> {
        let mut conditions = index::rows_of(def);
        let mut params = Vec::new();
        let mut narrowing = Vec::new();
        for filter in &self.filters {
            let column = column(def, &filter.field)?;
            let (clause, param) = compile_filter(def, filter, &column)?;
            conditions = format!("{conditions} AND {clause}");
            params.extend(param);
            narrowing.extend(column.narrowing(filter.op));
        }
        let sorted_by = column(def, self.sort.as_ref().map_or("id", |sort| &sort.field))?;
        let narrowest = narrowing
            .into_iter()
            .min_by_key(|(precision, _)| *precision)
            .map(|(_, index)| index);
        let index = match narrowest {
            None if self.filters.is_empty() => sorted_by.index,
            narrowest => narrowest,
        };
        let source = match index {
            Some(index) => index::docs_by(&index),
            None if table_size <= MAX_SCAN_DOCS => "docs".to_owned(),
            None => return Err(self.needs_index(def, table_size)),
        };
        let direction = match &self.sort {
            Some(sort) if sort.descending => "DESC",
            _ => "ASC",
        };
        let order = if sorted_by.sql == "id" {
            format!("id {direction}")
        } else {
            format!("{} {direction}, id {direction}", sorted_by.sql)
        };
        Ok(Compiled {
            source,
            conditions,
            params,
            order,
        })
    }

    /// Says why no index can answer this query and what would change that.
    fn needs_index(&self, def: &TableDef, table_size: i64) -> DbError {
        let own_field = |name: &str| def.field(name).map(|field| field.name.clone());
        let indexable = self
            .filters
            .iter()
            .find(|filter| filter.op.can_use_index() && def.field(&filter.field).is_some());
        let (unserved, could_index) = match indexable.or_else(|| self.filters.first()) {
            Some(filter) if filter.op.can_use_index() => (
                format!("The filter on `{}` has no index to use", filter.field),
                own_field(&filter.field),
            ),
            Some(filter) => (
                format!(
                    "The `{}` filter on `{}` cannot use an index (`ne` and `contains` never can)",
                    filter.op.name(),
                    filter.field
                ),
                None,
            ),
            None => {
                let field = self.sort.as_ref().map_or("id", |sort| &sort.field);
                (
                    format!("The sort on `{field}` has no index to use"),
                    own_field(field),
                )
            }
        };
        let mut indexed: Vec<String> = INDEXED_SYSTEM_FIELDS.map(str::to_owned).to_vec();
        indexed.extend(def.indexed_fields());
        DbError::QueryNeedsIndex {
            table: def.name.clone(),
            count: table_size,
            limit: MAX_SCAN_DOCS,
            unserved,
            indexed,
            could_index,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Compiled {
    /// The `docs` table, read through the index chosen for the query when
    /// one can answer it.
    source: String,
    /// What selects the table's documents, then every filter, with a `?` for
    /// each of `params`.
    conditions: String,
    pub params: Vec<SqlValue>,
    order: String,
}

impl Compiled {
    pub(crate) fn count_sql(&self) -> String {
        format!(
            "SELECT count(*) FROM {} WHERE {}",
            self.source, self.conditions
        )
    }

    /// Takes the limit and the offset as two more values after `params`.
    pub(crate) fn page_sql(&self) -> String {
        format!(
            "SELECT {DOC_COLUMNS} FROM {} WHERE {} ORDER BY {} LIMIT ? OFFSET ?",
            self.source, self.conditions, self.order
        )
    }
}

/// What a filter or sort reads: a value the database sets, or a field of
/// the document.
struct Column {
    sql: String,
    kind: FieldType,
    /// The index over this column, when it has one.
    index: Option<String>,
    unique: bool,
}

impl Column {
    fn system(name: &str, kind: FieldType, index: Option<&str>) -> Self {
        Self {
            sql: name.to_owned(),
            kind,
            index: index.map(str::to_owned),
            unique: name == "id",
        }
    }

    /// The index that answers a filter with `op` on this column, and how
    /// far it narrows the search: the lower, the fewer documents are left.
    fn narrowing(&self, op: Op) -> Option<(u8, String)> {
        let index = self.index.clone()?;
        let precision = match op {
            Op::Eq if self.unique => 0,
            Op::Eq => 1,
            Op::Gt | Op::Gte | Op::Lt | Op::Lte => 2,
            Op::Ne | Op::Contains => return None,
        };
        Some((precision, index))
    }
}

fn column(def: &TableDef, name: &str) -> Result<Column> {
    match name {
        "id" => Ok(Column::system(name, FieldType::Number, Some(index::BY_ID))),
        "version" => Ok(Column::system(name, FieldType::Number, None)),
        "created_at" => Ok(Column::system(
            name,
            FieldType::Datetime,
            Some(index::BY_CREATED_AT),
        )),
        "updated_at" => Ok(Column::system(
            name,
            FieldType::Datetime,
            Some(index::BY_UPDATED_AT),
        )),
        _ => {
            let field = def
                .field(name)
                .ok_or_else(|| def.unknown_field(name, true))?;
            Ok(Column {
                sql: index::value_of(&field.name),
                kind: field.kind.clone(),
                index: field.indexed.then(|| index::of_field(def, field)),
                unique: field.unique,
            })
        }
    }
}

fn compile_filter(
    def: &TableDef,
    filter: &Filter,
    column: &Column,
) -> Result<(String, Option<SqlValue>)> {
    let Column { sql, kind, .. } = column;
    let allowed = allowed_ops(kind);
    if !allowed.contains(&filter.op) {
        return Err(DbError::InvalidOperator {
            field: filter.field.clone(),
            op: filter.op.name().to_owned(),
            field_type: kind.describe(),
            allowed: allowed.iter().map(|op| op.name().to_owned()).collect(),
        });
    }
    if filter.value.is_null() {
        return match filter.op {
            Op::Eq => Ok((format!("{sql} IS NULL"), None)),
            Op::Ne => Ok((format!("{sql} IS NOT NULL"), None)),
            _ => Err(DbError::InvalidOperator {
                field: filter.field.clone(),
                op: filter.op.name().to_owned(),
                field_type: "null value".to_owned(),
                allowed: EQUALITY.iter().map(|op| op.name().to_owned()).collect(),
            }),
        };
    }
    let value = kind.coerce(&def.name, &filter.field, &filter.value)?;
    if let (Op::Contains, Value::String(needle)) = (filter.op, &value) {
        return Ok((
            format!("instr({LOWERCASE}({sql}), ?) > 0"),
            Some(SqlValue::Text(needle.to_lowercase())),
        ));
    }
    Ok((
        format!("{sql} {} ?", filter.op.sql()),
        Some(sql_value(&value)?),
    ))
}

/// Registers the SQL function `contains` relies on. SQLite's own `lower()`
/// only knows ASCII, so it would miss `Łódź` when asked for `łódź`.
pub(crate) fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        LOWERCASE,
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |call| {
            Ok(call
                .get::<Option<String>>(0)?
                .map(|text| text.to_lowercase()))
        },
    )
}

pub(crate) fn sql_value(value: &Value) -> Result<SqlValue> {
    match value {
        Value::String(text) => Ok(SqlValue::Text(text.clone())),
        Value::Bool(flag) => Ok(SqlValue::Integer(i64::from(*flag))),
        Value::Number(number) => number
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| number.as_f64().map(SqlValue::Real))
            .ok_or_else(|| DbError::Internal(format!("number {number} cannot be stored"))),
        other => Err(DbError::Internal(format!("cannot filter on {other}"))),
    }
}
