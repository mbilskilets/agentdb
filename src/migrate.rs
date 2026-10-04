//! Changes to the shape of the database: new tables, and changes to tables
//! that already hold documents. The schema and the stored documents change
//! together in one transaction, or not at all. Changes that would delete
//! data refuse unless `force` is set.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::change::ChangeKind;
use crate::db::{
    AgentDb, all_defs, check_ref_target, count_docs, doc_exists, from_json, load_def, read_doc,
    record, store_def, to_json,
};
use crate::error::{DbError, Result};
use crate::index;
use crate::schema::{Field, FieldType, TableDef, check_enum_value, check_name, validate_field};

/// How many documents a schema change reads or rewrites at a time. Until a
/// statement ends, SQLite keeps what the statement overwrote in memory, so a
/// single statement over a whole table needs memory in proportion to it.
const DOCS_PER_CHUNK: i64 = 500;

/// One change to the schema. Pass several to [`AgentDb::migrate`] to apply
/// them as a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SchemaChange {
    DefineTable {
        table: TableDef,
    },
    AddField {
        table: String,
        field: Field,
    },
    RenameTable {
        table: String,
        new_name: String,
    },
    RenameField {
        table: String,
        field: String,
        new_name: String,
    },
    /// Converts every stored value to the new type, or changes nothing if
    /// any value does not fit.
    ChangeType {
        table: String,
        field: String,
        to: FieldType,
    },
    SetRequired {
        table: String,
        field: String,
        required: bool,
    },
    /// Lets queries filter and sort by the field without reading every
    /// document, at the price of slightly slower writes.
    SetIndexed {
        table: String,
        field: String,
        indexed: bool,
    },
    /// Refuses two documents with the same value in the field. Changes
    /// nothing if such documents already exist.
    SetUnique {
        table: String,
        field: String,
        unique: bool,
    },
    AddEnumValue {
        table: String,
        field: String,
        value: String,
    },
    RemoveEnumValue {
        table: String,
        field: String,
        value: String,
    },
    /// Sets the description of a table, or of one of its fields. An empty
    /// description clears it.
    Describe {
        table: String,
        #[serde(default)]
        field: Option<String>,
        description: String,
    },
    RemoveField {
        table: String,
        field: String,
        #[serde(default)]
        force: bool,
    },
    DropTable {
        table: String,
        #[serde(default)]
        force: bool,
    },
}

impl AgentDb {
    /// Applies schema changes in order as one unit: if any step fails, none
    /// of them take effect.
    ///
    /// # Errors
    /// The failing step's error. With more than one change it is wrapped in
    /// [`DbError::StepFailed`], which says which step it was.
    pub fn migrate(&self, changes: &[SchemaChange]) -> Result<()> {
        self.write(|conn, at| {
            let events = changes
                .iter()
                .enumerate()
                .map(|(step, change)| {
                    let table = apply(conn, change)
                        .map_err(|source| source.at_step(step + 1, changes.len()))?;
                    record(conn, ChangeKind::Schema, &table, at, None)
                })
                .collect::<Result<Vec<_>>>()?;
            index::sync(conn)?;
            Ok(events)
        })?;
        Ok(())
    }

    /// Creates a table.
    ///
    /// # Errors
    /// Fails when the table exists, a name is invalid, or a reference field
    /// points to a table that does not exist.
    pub fn define_table(&self, def: &TableDef) -> Result<()> {
        self.migrate(&[SchemaChange::DefineTable { table: def.clone() }])
    }

    /// Adds a field to an existing table.
    ///
    /// # Errors
    /// Fails when the field exists, or when it is required and the table
    /// already holds documents.
    pub fn add_field(&self, table: &str, field: Field) -> Result<()> {
        self.migrate(&[SchemaChange::AddField {
            table: table.to_owned(),
            field,
        }])
    }

    /// Renames a table. Fields in other tables that link to it follow.
    ///
    /// # Errors
    /// Fails when the new name is invalid or already taken.
    pub fn rename_table(&self, table: &str, new_name: &str) -> Result<()> {
        self.migrate(&[SchemaChange::RenameTable {
            table: table.to_owned(),
            new_name: new_name.to_owned(),
        }])
    }

    /// Renames a field and moves every document's value to the new name.
    ///
    /// # Errors
    /// Fails when the field does not exist or the new name is invalid or taken.
    pub fn rename_field(&self, table: &str, field: &str, new_name: &str) -> Result<()> {
        self.migrate(&[SchemaChange::RenameField {
            table: table.to_owned(),
            field: field.to_owned(),
            new_name: new_name.to_owned(),
        }])
    }

    /// Changes a field's type and converts every stored value.
    ///
    /// # Errors
    /// [`DbError::CannotConvert`] when a stored value does not fit the new
    /// type; nothing is changed in that case.
    pub fn change_field_type(&self, table: &str, field: &str, to: FieldType) -> Result<()> {
        self.migrate(&[SchemaChange::ChangeType {
            table: table.to_owned(),
            field: field.to_owned(),
            to,
        }])
    }

    /// Makes a field required or optional.
    ///
    /// # Errors
    /// [`DbError::MissingValues`] when making it required while some
    /// documents have no value for it.
    pub fn set_required(&self, table: &str, field: &str, required: bool) -> Result<()> {
        self.migrate(&[SchemaChange::SetRequired {
            table: table.to_owned(),
            field: field.to_owned(),
            required,
        }])
    }

    /// Turns the index on a field on or off. A table with more than 1,000
    /// documents can only be filtered or sorted by indexed fields.
    ///
    /// # Errors
    /// [`DbError::TooManyIndexes`] when the table already has 10 indexed
    /// fields, [`DbError::IndexRequired`] when turning off the index of a
    /// unique field or a ref.
    pub fn set_indexed(&self, table: &str, field: &str, indexed: bool) -> Result<()> {
        self.migrate(&[SchemaChange::SetIndexed {
            table: table.to_owned(),
            field: field.to_owned(),
            indexed,
        }])
    }

    /// Makes a field unique, so no two documents may hold the same value in
    /// it, or lifts that rule. A unique field is always indexed.
    ///
    /// # Errors
    /// [`DbError::DuplicatesExist`] when documents already share a value;
    /// nothing is changed in that case.
    pub fn set_unique(&self, table: &str, field: &str, unique: bool) -> Result<()> {
        self.migrate(&[SchemaChange::SetUnique {
            table: table.to_owned(),
            field: field.to_owned(),
            unique,
        }])
    }

    /// Adds one more allowed value to an enum field.
    ///
    /// # Errors
    /// Fails when the field is not an enum or already allows the value.
    pub fn add_enum_value(&self, table: &str, field: &str, value: &str) -> Result<()> {
        self.migrate(&[SchemaChange::AddEnumValue {
            table: table.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
        }])
    }

    /// Removes an allowed value from an enum field.
    ///
    /// # Errors
    /// [`DbError::EnumValueInUse`] when documents still hold the value.
    pub fn remove_enum_value(&self, table: &str, field: &str, value: &str) -> Result<()> {
        self.migrate(&[SchemaChange::RemoveEnumValue {
            table: table.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
        }])
    }

    /// Says in plain words what a table holds, or what one of its fields
    /// means when `field` is given.
    ///
    /// # Errors
    /// Fails when the table or field does not exist.
    pub fn set_description(&self, table: &str, field: Option<&str>, text: &str) -> Result<()> {
        self.migrate(&[SchemaChange::Describe {
            table: table.to_owned(),
            field: field.map(str::to_owned),
            description: text.to_owned(),
        }])
    }

    /// Removes a field and its value from every document. When documents
    /// hold a value for it, this refuses unless `force` is true.
    ///
    /// # Errors
    /// [`DbError::WouldDestroy`] when data would be lost and `force` is false.
    pub fn remove_field(&self, table: &str, field: &str, force: bool) -> Result<()> {
        self.migrate(&[SchemaChange::RemoveField {
            table: table.to_owned(),
            field: field.to_owned(),
            force,
        }])
    }

    /// Deletes a table. When it holds documents, this refuses unless `force`
    /// is true.
    ///
    /// # Errors
    /// [`DbError::TableReferenced`] when another table links to it,
    /// [`DbError::WouldDestroy`] when documents would be lost and `force` is
    /// false.
    pub fn drop_table(&self, table: &str, force: bool) -> Result<()> {
        self.migrate(&[SchemaChange::DropTable {
            table: table.to_owned(),
            force,
        }])
    }
}

/// Applies one change and returns the name of the table it affected.
fn apply(conn: &Connection, change: &SchemaChange) -> Result<String> {
    use SchemaChange as C;
    let table = match change {
        C::DefineTable { table } => return define_table(conn, table).map(|()| table.name.clone()),
        C::RenameTable { table, new_name } => {
            return rename_table(conn, table, new_name).map(|()| new_name.clone());
        }
        C::AddField { table, field } => add_field(conn, table, field).map(|()| table),
        C::RenameField {
            table,
            field,
            new_name,
        } => rename_field(conn, table, field, new_name).map(|()| table),
        C::ChangeType { table, field, to } => change_type(conn, table, field, to).map(|()| table),
        C::SetRequired {
            table,
            field,
            required,
        } => set_required(conn, table, field, *required).map(|()| table),
        C::SetIndexed {
            table,
            field,
            indexed,
        } => set_indexed(conn, table, field, *indexed).map(|()| table),
        C::SetUnique {
            table,
            field,
            unique,
        } => set_unique(conn, table, field, *unique).map(|()| table),
        C::AddEnumValue {
            table,
            field,
            value,
        } => add_enum_value(conn, table, field, value).map(|()| table),
        C::RemoveEnumValue {
            table,
            field,
            value,
        } => remove_enum_value(conn, table, field, value).map(|()| table),
        C::Describe {
            table,
            field,
            description,
        } => describe(conn, table, field.as_deref(), description).map(|()| table),
        C::RemoveField {
            table,
            field,
            force,
        } => remove_field(conn, table, field, *force).map(|()| table),
        C::DropTable { table, force } => drop_table(conn, table, *force).map(|()| table),
    }?;
    Ok(table.clone())
}

fn define_table(conn: &Connection, def: &TableDef) -> Result<()> {
    def.validate()?;
    for field in &def.fields {
        check_ref_target(conn, &field.kind, Some(&def.name))?;
    }
    let mut def = def.clone();
    def.settle_indexes()?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO _tables (name, schema) VALUES (?1, ?2)",
        params![def.name, to_json(&def)?],
    )?;
    if inserted == 0 {
        return Err(DbError::TableExists {
            table: def.name.clone(),
        });
    }
    Ok(())
}

fn add_field(conn: &Connection, table: &str, field: &Field) -> Result<()> {
    validate_field(field)?;
    let mut def = load_def(conn, table)?;
    if def.field(&field.name).is_some() {
        return Err(DbError::FieldExists {
            table: table.to_owned(),
            field: field.name.clone(),
        });
    }
    check_ref_target(conn, &field.kind, Some(table))?;
    let count = count_docs(conn, table)?;
    if field.required && count > 0 {
        return Err(DbError::RequiredFieldOnExistingDocs {
            table: table.to_owned(),
            field: field.name.clone(),
            count,
        });
    }
    def.fields.push(field.clone());
    save_def(conn, &mut def)
}

fn rename_table(conn: &Connection, table: &str, new_name: &str) -> Result<()> {
    check_name(new_name)?;
    let mut def = load_def(conn, table)?;
    if all_defs(conn)?.iter().any(|other| other.name == new_name) {
        return Err(DbError::TableExists {
            table: new_name.to_owned(),
        });
    }
    new_name.clone_into(&mut def.name);
    conn.execute(
        "UPDATE _tables SET name = ?2, schema = ?3 WHERE name = ?1",
        params![table, new_name, to_json(&def)?],
    )?;
    in_chunks(conn, table, |after, last| {
        conn.execute(
            "UPDATE docs SET tbl = ?4 WHERE tbl = ?1 AND id > ?2 AND id <= ?3",
            params![table, after, last, new_name],
        )?;
        Ok(())
    })?;
    conn.execute(
        "UPDATE changes SET tbl = ?2 WHERE tbl = ?1",
        params![table, new_name],
    )?;
    conn.execute(
        "INSERT INTO _ids (name, next_id) SELECT ?2, next_id FROM _ids WHERE name = ?1
         ON CONFLICT (name) DO UPDATE SET next_id = max(next_id, excluded.next_id)",
        params![table, new_name],
    )?;
    for mut other in all_defs(conn)? {
        if retarget(&mut other, table, new_name) {
            save_def(conn, &mut other)?;
        }
    }
    Ok(())
}

fn rename_field(conn: &Connection, table: &str, field: &str, new_name: &str) -> Result<()> {
    let mut def = load_def(conn, table)?;
    if def.field(new_name).is_some() {
        return Err(DbError::FieldExists {
            table: table.to_owned(),
            field: new_name.to_owned(),
        });
    }
    let target = field_mut(&mut def, field)?;
    new_name.clone_into(&mut target.name);
    validate_field(target)?;
    save_def(conn, &mut def)?;
    in_chunks(conn, table, |after, last| {
        conn.execute(
            "UPDATE docs SET body = json_set(json_remove(body, ?4), ?5, body -> ?4)
             WHERE tbl = ?1 AND id > ?2 AND id <= ?3 AND json_type(body, ?4) IS NOT NULL",
            params![table, after, last, json_path(field), json_path(new_name)],
        )?;
        Ok(())
    })
}

fn change_type(conn: &Connection, table: &str, field: &str, to: &FieldType) -> Result<()> {
    let mut def = load_def(conn, table)?;
    check_ref_target(conn, to, Some(table))?;
    let target = field_mut(&mut def, field)?;
    target.kind = to.clone();
    validate_field(target)?;
    let unique = target.unique;
    save_def(conn, &mut def)?;
    let mut misfits = 0;
    let mut first_misfit = None;
    in_chunks(conn, table, |after, last| {
        for (id, value) in values_between(conn, table, field, after, last)? {
            if let Some(converted) = convert(conn, table, field, to, &value)? {
                store_value(conn, table, field, id, &converted)?;
            } else {
                misfits += 1;
                first_misfit.get_or_insert((id, value));
            }
        }
        Ok(())
    })?;
    match first_misfit {
        None if unique => check_no_duplicates(conn, table, field),
        None => Ok(()),
        Some((example_id, example)) => Err(DbError::CannotConvert {
            table: table.to_owned(),
            field: field.to_owned(),
            to: to.describe(),
            count: misfits,
            example_id,
            example: example.to_string(),
        }),
    }
}

/// The values of `field` in the documents with an id after `after` and up
/// to `last` that hold one, in id order.
fn values_between(
    conn: &Connection,
    table: &str,
    field: &str,
    after: i64,
    last: i64,
) -> Result<Vec<(i64, Value)>> {
    conn.prepare(
        "SELECT id, body -> ?4 FROM docs
         WHERE tbl = ?1 AND id > ?2 AND id <= ?3 AND json_type(body, ?4) IS NOT NULL
         ORDER BY id",
    )?
    .query_map(params![table, after, last, json_path(field)], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?
    .map(|row| {
        let (id, value) = row?;
        Ok((id, from_json(&value)?))
    })
    .collect()
}

fn store_value(conn: &Connection, table: &str, field: &str, id: i64, value: &Value) -> Result<()> {
    conn.execute(
        "UPDATE docs SET body = json_set(body, ?3, json(?4)) WHERE tbl = ?1 AND id = ?2",
        params![table, id, json_path(field), to_json(value)?],
    )?;
    Ok(())
}

/// The value as the new type would store it, or `None` when it does not fit.
fn convert(
    conn: &Connection,
    table: &str,
    field: &str,
    to: &FieldType,
    value: &Value,
) -> Result<Option<Value>> {
    let candidate = match (to, value) {
        (FieldType::Text, Value::Number(number)) => Some(Value::String(number.to_string())),
        (FieldType::Text, Value::Bool(flag)) => Some(Value::String(flag.to_string())),
        (FieldType::Number, Value::String(text)) => parse_number(text),
        (FieldType::Bool, Value::String(text)) => match text.trim().to_lowercase().as_str() {
            "true" | "yes" => Some(Value::Bool(true)),
            "false" | "no" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => Some(value.clone()),
    };
    let Some(stored) = candidate.and_then(|value| to.coerce(table, field, &value).ok()) else {
        return Ok(None);
    };
    if let (FieldType::Ref { table: target }, Some(id)) = (to, stored.as_i64())
        && !doc_exists(conn, target, id)?
    {
        return Ok(None);
    }
    Ok(Some(stored))
}

fn parse_number(text: &str) -> Option<Value> {
    let text = text.trim();
    text.parse::<i64>().map(Value::from).ok().or_else(|| {
        let number = text.parse::<f64>().ok()?;
        Number::from_f64(number).map(Value::Number)
    })
}

fn set_required(conn: &Connection, table: &str, field: &str, required: bool) -> Result<()> {
    let mut def = load_def(conn, table)?;
    field_mut(&mut def, field)?.required = required;
    let missing = count_docs(conn, table)? - count_set(conn, table, field)?;
    if required && missing > 0 {
        return Err(DbError::MissingValues {
            table: table.to_owned(),
            field: field.to_owned(),
            count: missing,
        });
    }
    save_def(conn, &mut def)
}

fn set_indexed(conn: &Connection, table: &str, field: &str, indexed: bool) -> Result<()> {
    let mut def = load_def(conn, table)?;
    let target = field_mut(&mut def, field)?;
    if let (false, Some(reason)) = (indexed, target.index_reason()) {
        return Err(DbError::IndexRequired {
            table: table.to_owned(),
            field: field.to_owned(),
            reason,
        });
    }
    target.indexed = indexed;
    save_def(conn, &mut def)
}

fn set_unique(conn: &Connection, table: &str, field: &str, unique: bool) -> Result<()> {
    let mut def = load_def(conn, table)?;
    field_mut(&mut def, field)?.unique = unique;
    if unique {
        check_no_duplicates(conn, table, field)?;
    }
    save_def(conn, &mut def)
}

/// Refuses when two documents of `table` hold the same value in `field`,
/// which the caller has already found in the schema.
fn check_no_duplicates(conn: &Connection, table: &str, field: &str) -> Result<()> {
    let value = index::value_of(field);
    let duplicate: Option<(i64, i64, i64)> = conn
        .query_row(
            &format!(
                "SELECT count(*), min(id), max(id) FROM docs
                 WHERE tbl = ?1 AND {value} IS NOT NULL
                 GROUP BY {value} HAVING count(*) > 1 ORDER BY min(id) LIMIT 1"
            ),
            [table],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((count, first_id, second_id)) = duplicate else {
        return Ok(());
    };
    let shared = read_doc(conn, table, first_id)?.fields.remove(field);
    Err(DbError::DuplicatesExist {
        table: table.to_owned(),
        field: field.to_owned(),
        value: shared.unwrap_or_default().to_string(),
        count,
        first_id,
        second_id,
    })
}

fn enum_values<'a>(def: &'a mut TableDef, table: &str, field: &str) -> Result<&'a mut Vec<String>> {
    match &mut field_mut(def, field)?.kind {
        FieldType::Enum { values } => Ok(values),
        _ => Err(DbError::NotAnEnum {
            table: table.to_owned(),
            field: field.to_owned(),
        }),
    }
}

fn add_enum_value(conn: &Connection, table: &str, field: &str, value: &str) -> Result<()> {
    let mut def = load_def(conn, table)?;
    let values = enum_values(&mut def, table, field)?;
    check_enum_value(field, value)?;
    if values.iter().any(|existing| existing == value) {
        return Err(DbError::EnumValueExists {
            table: table.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
        });
    }
    values.push(value.to_owned());
    save_def(conn, &mut def)
}

fn remove_enum_value(conn: &Connection, table: &str, field: &str, value: &str) -> Result<()> {
    let mut def = load_def(conn, table)?;
    let values = enum_values(&mut def, table, field)?;
    if !values.iter().any(|existing| existing == value) {
        return Err(DbError::EnumValueMissing {
            table: table.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
            allowed: values.clone(),
        });
    }
    if values.len() == 1 {
        return Err(DbError::EmptyEnum {
            field: field.to_owned(),
        });
    }
    let count: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM docs WHERE tbl = ?1 AND {} = ?2",
            index::value_of(field)
        ),
        params![table, value],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Err(DbError::EnumValueInUse {
            table: table.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
            count,
        });
    }
    values.retain(|existing| existing != value);
    save_def(conn, &mut def)
}

fn describe(conn: &Connection, table: &str, field: Option<&str>, description: &str) -> Result<()> {
    let mut def = load_def(conn, table)?;
    let text = (!description.trim().is_empty()).then(|| description.trim().to_owned());
    match field {
        Some(field) => field_mut(&mut def, field)?.description = text,
        None => def.description = text,
    }
    save_def(conn, &mut def)
}

fn remove_field(conn: &Connection, table: &str, field: &str, force: bool) -> Result<()> {
    let mut def = load_def(conn, table)?;
    field_mut(&mut def, field)?;
    let count = count_set(conn, table, field)?;
    if count > 0 && !force {
        return Err(DbError::WouldDestroy {
            what: format!("the `{field}` value of `{table}` documents"),
            count,
        });
    }
    def.fields.retain(|existing| existing.name != field);
    save_def(conn, &mut def)?;
    in_chunks(conn, table, |after, last| {
        conn.execute(
            "UPDATE docs SET body = json_remove(body, ?4)
             WHERE tbl = ?1 AND id > ?2 AND id <= ?3 AND json_type(body, ?4) IS NOT NULL",
            params![table, after, last, json_path(field)],
        )?;
        Ok(())
    })
}

fn drop_table(conn: &Connection, table: &str, force: bool) -> Result<()> {
    load_def(conn, table)?;
    for other in all_defs(conn)? {
        let link = other.fields.iter().find(|field| field.links_to(table));
        if let Some(link) = link
            && other.name != table
        {
            return Err(DbError::TableReferenced {
                table: table.to_owned(),
                by_table: other.name,
                by_field: link.name.clone(),
            });
        }
    }
    let count = count_docs(conn, table)?;
    if count > 0 && !force {
        return Err(DbError::WouldDestroy {
            what: format!("table `{table}` and everything in it"),
            count,
        });
    }
    in_chunks(conn, table, |after, last| {
        conn.execute(
            "DELETE FROM docs WHERE tbl = ?1 AND id > ?2 AND id <= ?3",
            params![table, after, last],
        )?;
        Ok(())
    })?;
    conn.execute("DELETE FROM _tables WHERE name = ?1", [table])?;
    Ok(())
}

/// Points every link to `from` at `to` instead. Returns whether any changed.
fn retarget(def: &mut TableDef, from: &str, to: &str) -> bool {
    let mut changed = false;
    for field in &mut def.fields {
        if let FieldType::Ref { table } = &mut field.kind
            && table == from
        {
            to.clone_into(table);
            changed = true;
        }
    }
    changed
}

fn field_mut<'a>(def: &'a mut TableDef, name: &str) -> Result<&'a mut Field> {
    let unknown = def.unknown_field(name, false);
    def.fields
        .iter_mut()
        .find(|field| field.name == name)
        .ok_or(unknown)
}

/// Stores a changed definition, with an index on every field that needs one.
fn save_def(conn: &Connection, def: &mut TableDef) -> Result<()> {
    def.settle_indexes()?;
    store_def(conn, def)
}

/// How many documents hold a value for `field`, which the caller has
/// already found in the schema.
fn count_set(conn: &Connection, table: &str, field: &str) -> Result<i64> {
    Ok(conn.query_row(
        &format!(
            "SELECT count(*) FROM docs WHERE tbl = ?1 AND {} IS NOT NULL",
            index::value_of(field)
        ),
        [table],
        |row| row.get(0),
    )?)
}

/// Calls `change` for one chunk of the table's documents after another, in
/// id order, with the id the chunk starts after and the id it ends at.
fn in_chunks(
    conn: &Connection,
    table: &str,
    mut change: impl FnMut(i64, i64) -> Result<()>,
) -> Result<()> {
    let mut after = 0;
    while let Some(last) = end_of_chunk(conn, table, after)? {
        change(after, last)?;
        after = last;
    }
    Ok(())
}

/// The id of the last of the next [`DOCS_PER_CHUNK`] documents after the id
/// `after`, or `None` when no document follows it.
fn end_of_chunk(conn: &Connection, table: &str, after: i64) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT max(id) FROM
         (SELECT id FROM docs WHERE tbl = ?1 AND id > ?2 ORDER BY id LIMIT ?3)",
        params![table, after, DOCS_PER_CHUNK],
        |row| row.get(0),
    )?)
}

/// Where SQLite's JSON functions find `field` in a document. Field names
/// hold only lowercase letters, digits and underscores, so none needs quoting.
fn json_path(field: &str) -> String {
    format!("$.{field}")
}
