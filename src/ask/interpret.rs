//! Assembles the model's picks into a [`Query`].

use serde_json::Value;
use time::Date;

use crate::query::{Filter, Op, Query, Sort};
use crate::schema::{Field, FieldType, TableDef};

use super::calendar::{
    self, AFTER_DATE, BEFORE_DATE, Bounds, MORE_THAN_N_AGO, ON_DATE, PERIODS, SINCE_DATE,
    WITHIN_LAST_N,
};
use super::candidates::Candidates;
use super::questions::{
    self, ASCENDING, BY_ID, BY_PROPERTY, IS_NOT, NO, ROLE_LIMIT, ROLE_SPAN, UNRESTRICTED,
    UNSUPPORTED, YES, id,
};
use super::reader::Reader;

/// A value, or the reason it could not be worked out, worded for the caller.
pub(super) type Built<T> = Result<T, String>;

/// A query together with its description in plain words.
pub(super) struct Understood {
    pub query: Query,
    pub description: String,
}

/// The table the request is about, unless it is a request `ask()` does not
/// answer.
pub(super) fn route<'a>(defs: &'a [TableDef], reader: &mut Reader<'_>) -> Built<&'a TableDef> {
    for (id, _, refusal) in UNSUPPORTED {
        if reader.yes(id) {
            return Err(refusal.to_owned());
        }
    }
    let table = reader.pick(id::TABLE);
    defs.iter()
        .find(|def| Some(def.name.as_str()) == table)
        .ok_or_else(|| {
            let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
            format!(
                "the request does not match any table. Tables: {}.",
                names.join(", ")
            )
        })
}

pub(super) fn interpret(
    def: &TableDef,
    reader: &mut Reader<'_>,
    found: &Candidates,
    today: Date,
) -> Built<Understood> {
    let table = &def.name;
    let mut query = Query::table(table);
    for field in &def.fields {
        query.filters.extend(field_filter(reader, table, field)?);
    }
    let numbers = read_numbers(reader, def, found)?;
    query.filters.extend(numbers.filters);
    query.limit = numbers.limit;
    for field in &def.fields {
        let by_id = matches!(field.kind, FieldType::Ref { .. })
            && reader.pick(&id::field(table, &field.name)) == Some(BY_ID);
        if by_id && !has_filter(&query, &field.name) {
            return Err(unclear(table, &format!("the id for `{}`", field.name)));
        }
    }
    let time = time_condition(reader, table, numbers.span, found, today)?;
    if let Some(time) = &time {
        query.filters.extend(time.filters.iter().cloned());
    }
    if let Some(field) = reader.pick(&id::missing(table)) {
        query.filters.push(filter(field, Op::Eq, Value::Null));
    }
    if let Some(field) = field_that_must_be_set(reader, &query) {
        query.filters.push(filter(field, Op::Ne, Value::Null));
    }
    if reader.yes(id::SORT) {
        let field = reader
            .pick(&id::sort_field(table))
            .ok_or_else(|| unclear(table, "the field to order by"))?;
        query.sort = Some(Sort {
            field: field.to_owned(),
            descending: reader.pick(id::SORT_DIRECTION) != Some(ASCENDING),
        });
    }
    let description = describe(&query, time.as_ref());
    Ok(Understood { query, description })
}

fn filter(field: &str, op: Op, value: Value) -> Filter {
    Filter {
        field: field.to_owned(),
        op,
        value,
    }
}

fn has_filter(query: &Query, field: &str) -> bool {
    query.filters.iter().any(|filter| filter.field == field)
}

fn unclear(table: &str, part: &str) -> String {
    format!(
        "the request is about `{table}`, but {part} could not be worked out from it. Use find() with an explicit query."
    )
}

/// The filter a field gets from its own questions. Number and date fields
/// have none: they are filled from the numbers and the time period.
fn field_filter(reader: &mut Reader<'_>, table: &str, field: &Field) -> Built<Option<Filter>> {
    let name = field.name.as_str();
    let id = id::field(table, name);
    Ok(match &field.kind {
        FieldType::Enum { values } => {
            let picked = reader.pick(&id);
            questions::enum_options(values)
                .find(|(option, _, _)| Some(option.as_str()) == picked)
                .map(|(_, op, value)| filter(name, op, Value::from(value)))
        }
        FieldType::Bool => match reader.pick(&id) {
            Some(YES) => Some(filter(name, Op::Eq, Value::Bool(true))),
            Some(NO) => Some(filter(name, Op::Eq, Value::Bool(false))),
            _ => None,
        },
        FieldType::Ref { table: target } => {
            if reader.pick(&id) == Some(BY_PROPERTY) {
                return Err(format!(
                    "ask() filters `{table}` by their own fields only, and this request describes their `{name}` by a property of `{target}`. Find that `{target}` document first, then filter `{name}` by its id."
                ));
            }
            None
        }
        FieldType::Text => text_filter(reader, table, name)?,
        FieldType::Number | FieldType::Datetime => None,
    })
}

/// Text is matched with `contains`. A query has no "does not contain", so a
/// request to exclude by text is refused rather than run as something else.
fn text_filter(reader: &mut Reader<'_>, table: &str, name: &str) -> Built<Option<Filter>> {
    if !reader.yes(&id::field(table, name)) {
        return Ok(None);
    }
    if reader.pick(&id::text_match(table, name)) == Some(IS_NOT) {
        return Err(format!(
            "ask() matches text with `contains` and cannot exclude `{table}` by the text of their `{name}`. Use find(): its `ne` operator excludes one exact value."
        ));
    }
    let value = reader
        .pick(&id::text_value(table, name))
        .ok_or_else(|| unclear(table, &format!("the text to match `{name}` against")))?;
    Ok(Some(filter(name, Op::Contains, Value::from(value))))
}

/// What the numbers in the request turned out to be.
#[derive(Default)]
struct Numbers {
    filters: Vec<Filter>,
    limit: Option<u32>,
    /// The count in "the last N days".
    span: Option<u32>,
}

fn read_numbers(reader: &mut Reader<'_>, def: &TableDef, found: &Candidates) -> Built<Numbers> {
    let mut numbers = Numbers::default();
    for (index, (label, value)) in found.numbers.iter().enumerate() {
        match reader.pick(&id::number_role(&def.name, index)) {
            None => {}
            Some(ROLE_LIMIT) => {
                let limit = whole(value).ok_or_else(|| {
                    unclear(&def.name, &format!("how many results \"{label}\" means"))
                })?;
                numbers.limit = Some(limit);
            }
            Some(ROLE_SPAN) => numbers.span = whole(value),
            Some(field) => {
                let is_ref = def
                    .field(field)
                    .is_some_and(|field| matches!(field.kind, FieldType::Ref { .. }));
                let asked = reader
                    .pick(&id::number_comparison(index))
                    .and_then(questions::comparison)
                    .unwrap_or(Op::Eq);
                let op = if is_ref && asked != Op::Ne {
                    Op::Eq
                } else {
                    asked
                };
                numbers.filters.push(filter(field, op, value.clone()));
            }
        }
    }
    Ok(numbers)
}

/// A number that can count results or days: whole and not negative.
fn whole(value: &Value) -> Option<u32> {
    value.as_u64().and_then(|count| u32::try_from(count).ok())
}

/// The field the request says must have a value. One that already has a
/// value filter is known to be set, so the model's answer is not relied on.
fn field_that_must_be_set<'a>(reader: &mut Reader<'a>, query: &Query) -> Option<&'a str> {
    let id = id::present(&query.table);
    let field = reader.peek(&id)?;
    if has_filter(query, field) {
        return None;
    }
    reader.pick(&id)
}

/// The request's time condition: the filters it becomes, and the words it
/// was understood as.
struct TimeCondition {
    field: String,
    phrase: String,
    filters: Vec<Filter>,
}

fn time_condition(
    reader: &mut Reader<'_>,
    table: &str,
    span: Option<u32>,
    found: &Candidates,
    today: Date,
) -> Built<Option<TimeCondition>> {
    let Some(period) = reader
        .pick(id::PERIOD)
        .filter(|period| *period != UNRESTRICTED)
    else {
        return Ok(None);
    };
    let field = reader
        .pick(&id::period_field(table))
        .ok_or_else(|| unclear(table, "which date the time condition applies to"))?;
    let (bounds, phrase) = period_bounds(reader, period, span, found, today)
        .ok_or_else(|| unclear(table, "the time period"))?;
    Ok(Some(TimeCondition {
        field: field.to_owned(),
        phrase,
        filters: bounds
            .into_iter()
            .map(|(op, date)| filter(field, op, Value::String(date.to_string())))
            .collect(),
    }))
}

/// The bounds of `period`, and the period in words.
fn period_bounds(
    reader: &mut Reader<'_>,
    period: &str,
    span: Option<u32>,
    found: &Candidates,
    today: Date,
) -> Option<(Bounds, String)> {
    match period {
        WITHIN_LAST_N | MORE_THAN_N_AGO => {
            let unit = reader.pick(id::PERIOD_UNIT)?;
            let count = span?;
            let phrase = if period == WITHIN_LAST_N {
                format!("within the last {count} {unit}")
            } else {
                format!("more than {count} {unit} ago")
            };
            Some((calendar::counted(period, today, count, unit)?, phrase))
        }
        ON_DATE | BEFORE_DATE | AFTER_DATE | SINCE_DATE => {
            let label = reader.pick(id::PERIOD_DATE)?;
            let phrase = match period {
                ON_DATE => format!("on {label}"),
                BEFORE_DATE => format!("before {label}"),
                AFTER_DATE => format!("after {label}"),
                _ => format!("on or after {label}"),
            };
            Some((calendar::dated(period, found.date(label)?)?, phrase))
        }
        named => {
            let (_, description) = PERIODS.iter().find(|(name, _)| *name == named)?;
            Some((calendar::named(named, today)?, (*description).to_owned()))
        }
    }
}

/// Says in plain words what `query` asks for, so the model can compare it
/// with the request.
fn describe(query: &Query, time: Option<&TimeCondition>) -> String {
    let mut conditions: Vec<String> = query
        .filters
        .iter()
        .filter(|filter| time.is_none_or(|time| !time.filters.contains(filter)))
        .map(|filter| {
            let Filter { field, op, value } = filter;
            match (op, value) {
                (Op::Eq, Value::Null) => format!("they have no {field}"),
                (Op::Ne, Value::Null) => format!("they have a {field}"),
                (Op::Eq, _) => format!("{field} is {value}"),
                (Op::Ne, _) => format!("{field} is not {value}"),
                (Op::Gt, _) => format!("{field} is more than {value}"),
                (Op::Gte, _) => format!("{field} is {value} or more"),
                (Op::Lt, _) => format!("{field} is less than {value}"),
                (Op::Lte, _) => format!("{field} is {value} or less"),
                (Op::Contains, _) => format!("{field} contains {value}"),
            }
        })
        .collect();
    if let Some(time) = time {
        conditions.push(format!("{} is {}", time.field, time.phrase));
    }
    let mut description = format!("all {}", query.table);
    if !conditions.is_empty() {
        description = format!("{} where {}", query.table, conditions.join(" and "));
    }
    if let Some(sort) = &query.sort {
        let direction = if sort.descending {
            "largest first"
        } else {
            "smallest first"
        };
        description = format!("{description}, ordered by {} ({direction})", sort.field);
    }
    if let Some(limit) = query.limit {
        description = format!("{description}, at most {limit} of them");
    }
    description
}
