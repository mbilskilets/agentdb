use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use time::format_description::well_known::{Iso8601, Rfc3339};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::error::{DbError, Result};

pub(crate) const SYSTEM_FIELDS: [&str; 4] = ["id", "version", "created_at", "updated_at"];
const MAX_NAME_LEN: usize = 64;
const MIN_SIMILARITY: f64 = 0.6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Number,
    Bool,
    /// RFC 3339 timestamp or a plain `YYYY-MM-DD` date. Stored in UTC.
    Datetime,
    Enum {
        values: Vec<String>,
    },
    /// The id of a document in another table.
    Ref {
        table: String,
    },
}

impl FieldType {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Text => "text".to_owned(),
            Self::Number => "number".to_owned(),
            Self::Bool => "bool (true or false)".to_owned(),
            Self::Datetime => {
                "datetime (like \"2026-10-03T14:30:00Z\" or \"2026-10-03\")".to_owned()
            }
            Self::Enum { values } => format!("one of: {}", values.join(", ")),
            Self::Ref { table } => format!("the numeric id of a `{table}` document"),
        }
    }

    /// Checks `value` against this type and returns it in stored form.
    pub(crate) fn coerce(&self, table: &str, field: &str, value: &Value) -> Result<Value> {
        let coerced = match (self, value) {
            (Self::Text, Value::String(_))
            | (Self::Bool, Value::Bool(_))
            | (Self::Number, Value::Number(_)) => Some(value.clone()),
            (Self::Datetime, Value::String(text)) => normalize_datetime(text).map(Value::String),
            (Self::Enum { values }, Value::String(text)) => {
                values.contains(text).then(|| value.clone())
            }
            (Self::Ref { .. }, Value::Number(number)) => number.as_i64().map(Value::from),
            _ => None,
        };
        coerced.ok_or_else(|| DbError::WrongType {
            table: table.to_owned(),
            field: field.to_owned(),
            expected: self.describe(),
            got: value.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(flatten)]
    pub kind: FieldType,
    pub required: bool,
    /// What the field means, in plain words. Shown by `describe()` and used
    /// to understand English requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Field {
    pub fn new(name: impl Into<String>, kind: FieldType, required: bool) -> Self {
        Self {
            name: name.into(),
            kind,
            required,
            description: None,
        }
    }

    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDef {
    pub name: String,
    /// What a document in this table is, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<Field>,
}

impl TableDef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            fields: Vec::new(),
        }
    }

    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    #[must_use]
    pub fn required(self, name: impl Into<String>, kind: FieldType) -> Self {
        self.with(Field::new(name, kind, true))
    }

    #[must_use]
    pub fn optional(self, name: impl Into<String>, kind: FieldType) -> Self {
        self.with(Field::new(name, kind, false))
    }

    /// Adds a fully built field, such as one with a description.
    #[must_use]
    pub fn with(mut self, field: Field) -> Self {
        self.fields.push(field);
        self
    }

    pub(crate) fn validate(&self) -> Result<()> {
        check_name(&self.name)?;
        for (index, field) in self.fields.iter().enumerate() {
            validate_field(field)?;
            if self.fields.iter().take(index).any(|f| f.name == field.name) {
                return Err(DbError::FieldExists {
                    table: self.name.clone(),
                    field: field.name.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.name == name)
    }

    pub(crate) fn unknown_field(&self, name: &str, with_system: bool) -> DbError {
        let mut available: Vec<String> = self.fields.iter().map(|f| f.name.clone()).collect();
        if with_system {
            available.extend(SYSTEM_FIELDS.iter().map(|&f| f.to_owned()));
        }
        DbError::UnknownField {
            table: self.name.clone(),
            field: name.to_owned(),
            suggestion: closest(name, &available),
            available,
        }
    }

    /// Checks every entry of `doc` against the schema and returns the entries
    /// in stored form. A `null` value means "unset" and is kept as `null`.
    pub(crate) fn check(&self, doc: Map<String, Value>) -> Result<Map<String, Value>> {
        let mut checked = Map::new();
        for (name, value) in doc {
            if SYSTEM_FIELDS.contains(&name.as_str()) {
                return Err(DbError::ReservedField { field: name });
            }
            let field = self
                .field(&name)
                .ok_or_else(|| self.unknown_field(&name, false))?;
            let stored = if value.is_null() {
                Value::Null
            } else {
                field.kind.coerce(&self.name, &name, &value)?
            };
            checked.insert(name, stored);
        }
        Ok(checked)
    }

    pub(crate) fn check_required(&self, doc: &Map<String, Value>) -> Result<()> {
        let missing: Vec<String> = self
            .fields
            .iter()
            .filter(|field| field.required && doc.get(&field.name).is_none_or(Value::is_null))
            .map(|field| field.name.clone())
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(DbError::MissingRequired {
                table: self.name.clone(),
                fields: missing,
            })
        }
    }

    /// The `(field, target table, id)` of every reference set in `doc`.
    pub(crate) fn references<'a>(
        &'a self,
        doc: &Map<String, Value>,
    ) -> Vec<(&'a str, &'a str, i64)> {
        self.fields
            .iter()
            .filter_map(|field| {
                let FieldType::Ref { table } = &field.kind else {
                    return None;
                };
                let id = doc.get(&field.name)?.as_i64()?;
                Some((field.name.as_str(), table.as_str(), id))
            })
            .collect()
    }
}

pub(crate) fn validate_field(field: &Field) -> Result<()> {
    check_name(&field.name)?;
    if SYSTEM_FIELDS.contains(&field.name.as_str()) {
        return Err(DbError::ReservedField {
            field: field.name.clone(),
        });
    }
    match &field.kind {
        FieldType::Enum { values } => check_enum_values(&field.name, values),
        _ => Ok(()),
    }
}

fn check_enum_values(field: &str, values: &[String]) -> Result<()> {
    if values.is_empty() {
        return Err(DbError::EmptyEnum {
            field: field.to_owned(),
        });
    }
    for (index, value) in values.iter().enumerate() {
        check_enum_value(field, value)?;
        if values.iter().take(index).any(|earlier| earlier == value) {
            return Err(DbError::RepeatedEnumValue {
                field: field.to_owned(),
                value: value.clone(),
            });
        }
    }
    Ok(())
}

pub(crate) fn check_enum_value(field: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return Err(DbError::BlankEnumValue {
            field: field.to_owned(),
        });
    }
    Ok(())
}

pub(crate) fn check_name(name: &str) -> Result<()> {
    let starts_with_letter = name.chars().next().is_some_and(|c| c.is_ascii_lowercase());
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if starts_with_letter && valid_chars && name.len() <= MAX_NAME_LEN {
        Ok(())
    } else {
        Err(DbError::InvalidName {
            name: name.to_owned(),
        })
    }
}

/// The option most similar to `name`, if any is close enough to be a typo.
pub(crate) fn closest(name: &str, options: &[String]) -> Option<String> {
    options
        .iter()
        .map(|option| (strsim::normalized_damerau_levenshtein(name, option), option))
        .filter(|(score, _)| *score >= MIN_SIMILARITY)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, option)| option.clone())
}

/// Parses a timestamp or plain date and renders it as UTC with whole seconds,
/// so stored values compare correctly as text.
pub(crate) fn normalize_datetime(text: &str) -> Option<String> {
    let parsed = OffsetDateTime::parse(text, &Rfc3339).ok().or_else(|| {
        Date::parse(text, &Iso8601::DATE)
            .ok()
            .map(|date| date.midnight().assume_utc())
    })?;
    format_utc(parsed)
}

pub(crate) fn format_utc(moment: OffsetDateTime) -> Option<String> {
    moment
        .to_offset(UtcOffset::UTC)
        .replace_nanosecond(0)
        .ok()?
        .format(&Rfc3339)
        .ok()
}
