use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DbError, Result};
use crate::schema::{FieldType, TableDef};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;
const LOWERCASE: &str = "agentdb_lowercase";

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

    pub(crate) fn compile(&self, def: &TableDef) -> Result<Compiled> {
        let mut conditions = String::new();
        let mut params = Vec::new();
        for filter in &self.filters {
            let (clause, param) = compile_filter(def, filter)?;
            conditions = format!("{conditions} AND {clause}");
            params.extend(param);
        }
        let order = match &self.sort {
            Some(sort) => {
                let (column, _) = column(def, &sort.field)?;
                let direction = if sort.descending { "DESC" } else { "ASC" };
                format!("{column} {direction}, id ASC")
            }
            None => "id ASC".to_owned(),
        };
        Ok(Compiled {
            conditions,
            params,
            order,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Compiled {
    /// Zero or more ` AND ...` clauses, each using `?` placeholders.
    pub conditions: String,
    pub params: Vec<SqlValue>,
    pub order: String,
}

/// The SQL expression and type for a field. Field names come from the schema,
/// never from the caller, so they are safe to place in the SQL text.
fn column(def: &TableDef, name: &str) -> Result<(String, FieldType)> {
    match name {
        "id" | "version" => Ok((name.to_owned(), FieldType::Number)),
        "created_at" | "updated_at" => Ok((name.to_owned(), FieldType::Datetime)),
        _ => {
            let field = def
                .field(name)
                .ok_or_else(|| def.unknown_field(name, true))?;
            Ok((
                format!("json_extract(body, '$.{}')", field.name),
                field.kind.clone(),
            ))
        }
    }
}

fn compile_filter(def: &TableDef, filter: &Filter) -> Result<(String, Option<SqlValue>)> {
    let (column, kind) = column(def, &filter.field)?;
    let allowed = allowed_ops(&kind);
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
            Op::Eq => Ok((format!("{column} IS NULL"), None)),
            Op::Ne => Ok((format!("{column} IS NOT NULL"), None)),
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
            format!("instr({LOWERCASE}({column}), ?) > 0"),
            Some(SqlValue::Text(needle.to_lowercase())),
        ));
    }
    Ok((
        format!("{column} {} ?", filter.op.sql()),
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

fn sql_value(value: &Value) -> Result<SqlValue> {
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
