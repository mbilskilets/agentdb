//! Turns an English request into a [`Query`].
//!
//! The model cannot write a query. This module lists every choice the schema
//! allows, asks the model to pick in one request, and assembles the picks.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use time::format_description::well_known::Iso8601;
use time::{Date, Duration, Month};

use crate::db::Page;
use crate::error::Result;
use crate::jev::{Answer, Judge, Question, Usage};
use crate::query::{Filter, Op, Query, Sort};
use crate::schema::{Field, FieldType, TableDef};

const MIN_CONFIDENCE: f64 = 0.6;
const NONE: &str = "none";
const MAX_PHRASE_WORDS: usize = 3;
const MAX_PHRASES: usize = 150;
const UNITS: [&str; 4] = ["day", "week", "month", "year"];
const COUNT_WORDS: [(&str, i64); 12] = [
    ("a", 1),
    ("an", 1),
    ("one", 1),
    ("two", 2),
    ("three", 3),
    ("four", 4),
    ("five", 5),
    ("six", 6),
    ("seven", 7),
    ("eight", 8),
    ("nine", 9),
    ("ten", 10),
];

/// The result of [`crate::AgentDb::ask`]. `page` is present only when the
/// query was run; otherwise `refusal` says why not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Asked {
    /// How the request was understood. Present even when not run, so the
    /// caller can check it and pass it to `find()`.
    pub query: Option<Query>,
    /// The weakest of the model's answers that shaped the query, 0 to 1.
    pub confidence: f64,
    pub refusal: Option<String>,
    pub page: Option<Page>,
    pub usage: Usage,
}

pub(crate) struct Plan {
    pub query: Option<Query>,
    pub confidence: f64,
    pub refusal: Option<String>,
    pub usage: Usage,
}

type Questions = BTreeMap<String, Question>;
/// A query, or the reason it could not be built, worded for the caller.
type Built<T> = std::result::Result<T, String>;

/// Values found in the request text. The model picks among these because it
/// cannot produce a value of its own.
struct Candidates {
    numbers: Vec<(String, Value)>,
    dates: Vec<(String, Date)>,
    phrases: Vec<String>,
}

impl Candidates {
    fn find(text: &str) -> Self {
        let words: Vec<&str> = text
            .split_whitespace()
            .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
            .filter(|word| !word.is_empty())
            .collect();
        let mut numbers: Vec<(String, Value)> = Vec::new();
        for (index, word) in words.iter().enumerate() {
            let next = words.get(index + 1).copied().unwrap_or_default();
            let found = parse_number(word)
                .map(|value| ((*word).to_owned(), value))
                .or_else(|| counted_unit(word, next));
            if let Some((label, value)) = found
                && !numbers.iter().any(|(known, _)| *known == label)
            {
                numbers.push((label, value));
            }
        }
        let dates = words
            .iter()
            .filter_map(|word| Some(((*word).to_owned(), Date::parse(word, &Iso8601::DATE).ok()?)))
            .collect();
        Self {
            numbers,
            dates,
            phrases: phrases(&words),
        }
    }

    fn date(&self, label: &str) -> Option<Date> {
        let (_, date) = self.dates.iter().find(|(name, _)| name == label)?;
        Some(*date)
    }
}

/// Reads "a year" or "two weeks" as a count, so "a year ago" has a number.
fn counted_unit(word: &str, next: &str) -> Option<(String, Value)> {
    let unit = next.to_lowercase();
    let is_unit = UNITS
        .iter()
        .any(|name| unit == *name || unit.strip_suffix('s') == Some(name));
    let (_, count) = COUNT_WORDS
        .iter()
        .find(|(name, _)| *name == word.to_lowercase())?;
    is_unit.then(|| (format!("{word} {next}"), Value::from(*count)))
}

/// Every run of one to three consecutive words, without repeats.
fn phrases(words: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let runs = (1..=MAX_PHRASE_WORDS).flat_map(|length| words.windows(length));
    for phrase in runs.map(|run| run.join(" ")) {
        if phrase != NONE && !found.contains(&phrase) && found.len() < MAX_PHRASES {
            found.push(phrase);
        }
    }
    found
}

/// Reads `1500`, `1,500`, `$1500`, `5k` and `1.5m`.
fn parse_number(word: &str) -> Option<Value> {
    let cleaned = word.replace(',', "").to_lowercase();
    let (digits, scale) = match cleaned.strip_suffix('k') {
        Some(rest) => (rest, 1_000_i64),
        None => match cleaned.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000),
            None => (cleaned.as_str(), 1),
        },
    };
    if let Ok(whole) = digits.parse::<i64>() {
        return whole.checked_mul(scale).map(Value::from);
    }
    let fraction = digits.parse::<f64>().ok()?;
    let scaled = if scale == 1 {
        fraction
    } else if scale == 1_000 {
        fraction * 1e3
    } else {
        fraction * 1e6
    };
    serde_json::Number::from_f64(scaled).map(Value::Number)
}

fn noul(instructions: impl Into<String>) -> Question {
    Question::Noul {
        instructions: instructions.into(),
    }
}

fn choice<N, D>(
    instructions: impl Into<String>,
    options: impl IntoIterator<Item = (N, D)>,
) -> Question
where
    N: Into<String>,
    D: Into<String>,
{
    Question::Choice {
        instructions: instructions.into(),
        criteria: options
            .into_iter()
            .map(|(name, description)| (name.into(), description.into()))
            .collect(),
    }
}

/// A choice among values found in the request, plus a way to say none fits.
fn value_choice(instructions: String, labels: impl IntoIterator<Item = String>) -> Question {
    let options = labels
        .into_iter()
        .map(|label| {
            let description = format!("the request means \"{label}\"");
            (label, description)
        })
        .chain([(
            NONE.to_owned(),
            "none of the other options is the value meant".to_owned(),
        )]);
    choice(instructions, options)
}

const COMPARISONS: [(&str, &str); 6] = [
    ("is", "exactly equal to the number"),
    ("is_not", "anything other than the number"),
    (
        "more_than",
        "strictly greater than the number: over, above, more than",
    ),
    (
        "at_least",
        "the number or greater: at least, minimum, or more, from, between ... and",
    ),
    (
        "less_than",
        "strictly less than the number: under, below, less than",
    ),
    (
        "at_most",
        "the number or smaller: at most, maximum, up to, or less, between ... and",
    ),
];
const MATCHES: [(&str, &str); 2] = [
    ("is", "it is, matches, or is named the value"),
    (
        "is_not",
        "it is anything other than the value: not, non-, except",
    ),
];
const PERIODS: [(&str, &str); 14] = [
    ("today", "during today"),
    ("yesterday", "during yesterday"),
    ("this_week", "during the current week"),
    ("last_week", "during the previous week"),
    ("this_month", "during the current month"),
    ("last_month", "during the previous month"),
    ("this_year", "during the current year"),
    ("last_year", "during the previous year"),
    (
        "within_last_n",
        "within the last or past N days, weeks, months or years, counted back from now",
    ),
    (
        "more_than_n_ago",
        "longer ago than N days, weeks, months or years: more than N ago, over N ago",
    ),
    (
        "on_date",
        "on one specific calendar date that the request writes out",
    ),
    (
        "before_date",
        "before a specific calendar date that the request writes out",
    ),
    (
        "after_date",
        "after a specific calendar date that the request writes out",
    ),
    (
        "since_date",
        "on or after a specific calendar date that the request writes out: since, from",
    ),
];
const UNSUPPORTED: [(&str, &str); 4] = [
    (
        "write",
        "ask() only reads. This looks like a request to change data: use insert(), update() or delete().",
    ),
    (
        "statistic",
        "ask() returns documents and how many matched, not sums or averages. Fetch the documents and calculate from them.",
    ),
    (
        "either_or",
        "ask() cannot combine conditions with OR. Call ask() once per alternative.",
    ),
    (
        "relational",
        "ask() cannot compare documents with each other. Fetch the documents and compare them yourself.",
    ),
];

fn describe_field(field: &Field) -> String {
    let mut text = field.name.clone();
    if let FieldType::Enum { values } = &field.kind {
        text = format!("{text} ({})", values.join(", "));
    }
    match &field.description {
        Some(description) => format!("{text}: {description}"),
        None => text,
    }
}

fn describe_table(def: &TableDef) -> String {
    let fields: Vec<String> = def.fields.iter().map(describe_field).collect();
    let fields = fields.join("; ");
    match &def.description {
        Some(description) => format!("{description}. Fields: {fields}"),
        None => format!("records with the fields: {fields}"),
    }
}

fn global_questions(defs: &[TableDef], found: &Candidates) -> Questions {
    let tables = defs
        .iter()
        .map(|def| (def.name.clone(), describe_table(def)))
        .chain([(
            NONE.to_owned(),
            "the request is not about any of these record types".to_owned(),
        )]);
    let mut questions = Questions::from([
        (
            "table".to_owned(),
            choice(
                "Which kind of record does the request ask for? A request may name records by one of their listed values, such as a status or a role.",
                tables,
            ),
        ),
        (
            "write".to_owned(),
            noul(
                "Does the request ask to create, change, or delete records, rather than only look them up?",
            ),
        ),
        (
            "statistic".to_owned(),
            noul(
                "Does the request ask for a calculated figure such as a sum, an average or a total amount? Answer no if it asks for records, for a ranking such as the cheapest or the top 5, or for how many records there are.",
            ),
        ),
        (
            "either_or".to_owned(),
            noul(
                "Does the request accept records that meet one condition OR a different condition, as alternatives? Answer no if every condition must hold together.",
            ),
        ),
        (
            "relational".to_owned(),
            noul(
                "Does the request compare one record's value with another record's value or with a group average, as in \"earn more than their manager\" or \"above average\"? Answer no for sorting, ranking, top N, cheapest or biggest, and for comparisons with a fixed number.",
            ),
        ),
        (
            "sort".to_owned(),
            noul(
                "Does the request explicitly ask for the results to be ordered or ranked, with words like sorted, ordered, top, highest, lowest, biggest, cheapest, newest, oldest, most or least?",
            ),
        ),
        (
            "sort.dir".to_owned(),
            choice(
                "In which direction does the request order the results?",
                [
                    (
                        "descending",
                        "largest, highest, newest or most recent first",
                    ),
                    (
                        "ascending",
                        "smallest, lowest, cheapest, oldest or earliest first",
                    ),
                ],
            ),
        ),
    ]);
    questions.extend(period_questions(found));
    for (index, (label, _)) in found.numbers.iter().enumerate() {
        questions.insert(
            format!("num.{index}.op"),
            choice(format!("The request mentions \"{label}\". How does it compare something against that number?"), COMPARISONS),
        );
    }
    questions
}

fn period_questions(found: &Candidates) -> Questions {
    Questions::from([
        (
            "period".to_owned(),
            choice(
                "Which time period does the request restrict the records to?",
                PERIODS
                    .iter()
                    .copied()
                    .chain([(NONE, "the request has no time condition")]),
            ),
        ),
        (
            "period.unit".to_owned(),
            choice(
                "Which unit of time does the request count in?",
                [
                    ("days", "days"),
                    ("weeks", "weeks"),
                    ("months", "months"),
                    ("years", "years"),
                ],
            ),
        ),
        (
            "period.date".to_owned(),
            value_choice(
                "Which calendar date does the request's time condition use?".to_owned(),
                found.dates.iter().map(|(label, _)| label.clone()),
            ),
        ),
    ])
}

fn date_columns(def: &TableDef) -> impl Iterator<Item = &str> {
    def.fields
        .iter()
        .filter(|field| field.kind == FieldType::Datetime)
        .map(|field| field.name.as_str())
        .chain(["created_at", "updated_at"])
}

fn table_questions(questions: &mut Questions, def: &TableDef, found: &Candidates) {
    let table = &def.name;
    for field in &def.fields {
        field_questions(questions, table, field, found);
    }
    let field_names = def.fields.iter().map(|field| field.name.as_str());
    questions.insert(
        format!("{table}.sort.field"),
        choice(
            format!("Which property of the {table} does the request order or rank them by?"),
            field_names
                .chain(["id", "created_at", "updated_at"])
                .map(|name| (name.to_owned(), format!("ordered by `{name}`")))
                .chain([(NONE.to_owned(), "no ordering is requested".to_owned())]),
        ),
    );
    questions.insert(
        format!("{table}.date_field"),
        choice(
            format!("Which date of the {table} does the request's time condition apply to?"),
            date_columns(def)
                .map(|name| {
                    (
                        name.to_owned(),
                        format!("the time condition is about `{name}`"),
                    )
                })
                .chain([(
                    NONE.to_owned(),
                    "the request has no time condition".to_owned(),
                )]),
        ),
    );
    for (index, (label, _)) in found.numbers.iter().enumerate() {
        questions.insert(format!("{table}.num.{index}"), number_role(def, label));
    }
    let nullable = || def.fields.iter().filter(|field| can_be_empty(field));
    questions.insert(
        format!("{table}.missing"),
        choice(
            format!("Does the request ask only for {table} that lack something, with words like without, no, never, missing or not yet? If so, what do they lack?"),
            nullable()
                .map(|field| (field.name.clone(), format!("only {table} that have no `{}`", field.name)))
                .chain([(NONE.to_owned(), "the request does not ask for records that lack something".to_owned())]),
        ),
    );
    questions.insert(
        format!("{table}.present"),
        choice(
            format!("Does the request say outright that the {table} must have something, whatever its value, with words like \"that have\", \"with a\" or \"having\"? If so, what must they have?"),
            nullable()
                .map(|field| (field.name.clone(), format!("only {table} that have some `{}`, whatever it is", field.name)))
                .chain([(NONE.to_owned(), "the request does not say this, which is the usual case".to_owned())]),
        ),
    );
}

/// Asks what one number in the request is for: a field's value, the result
/// limit, or a count of days in a time span.
fn number_role(def: &TableDef, label: &str) -> Question {
    let table = &def.name;
    let fields = def.fields.iter().filter_map(|field| match &field.kind {
        FieldType::Number => Some((
            field.name.clone(),
            format!(
                "it is a value that the `{}` of the {table} is compared against",
                field.name
            ),
        )),
        FieldType::Ref { table: target } => Some((
            field.name.clone(),
            format!(
                "it is the numeric id of the {table}' `{}` (one of the {target})",
                field.name
            ),
        )),
        _ => None,
    });
    let others = [
        ("id", format!("it is the id of the {table} themselves")),
        (
            "limit",
            "it is how many results to return, as in top 5 or first 10".to_owned(),
        ),
        (
            "period",
            "it counts days, weeks, months or years in a time span".to_owned(),
        ),
        (NONE, "it is something else".to_owned()),
    ];
    choice(
        format!("The request mentions \"{label}\". What is that number?"),
        fields.chain(others.map(|(name, description)| (name.to_owned(), description))),
    )
}

fn field_questions(questions: &mut Questions, table: &str, field: &Field, found: &Candidates) {
    let name = &field.name;
    let id = format!("{table}.{name}");
    match &field.kind {
        FieldType::Enum { values } => {
            let wanted = values.iter().map(|value| {
                (
                    value.clone(),
                    format!("only {table} whose `{name}` is {value}"),
                )
            });
            let excluded = values.iter().map(|value| {
                (
                    format!("not_{value}"),
                    format!("only {table} whose `{name}` is anything except {value}"),
                )
            });
            let unrestricted = (
                NONE.to_owned(),
                format!("the request does not restrict `{name}`"),
            );
            questions.insert(
                id,
                choice(
                    format!("Which `{name}` does the request restrict the {table} to?"),
                    wanted.chain(excluded).chain([unrestricted]),
                ),
            );
        }
        FieldType::Bool => {
            questions.insert(
                id,
                choice(
                    format!("Does the request restrict the {table} by whether they are `{name}`?"),
                    [
                        ("yes", format!("only {table} that are `{name}`")),
                        (
                            "no",
                            format!("only {table} that are not `{name}`: non-{name}, not {name}"),
                        ),
                        (NONE, format!("the request does not mention `{name}`")),
                    ],
                ),
            );
        }
        FieldType::Ref { table: target } => {
            questions.insert(
                id,
                choice(
                    format!("Does the request restrict the {table} by their `{name}`, which links each of them to one of the {target}?"),
                    [
                        ("by_id", format!("yes, by a numeric id, such as \"{name} 7\"")),
                        ("by_property", format!("yes, the request talks about the `{name}` or the {target} of the {table} and describes it by name or by another property")),
                        (NONE, format!("no, the request does not talk about a `{name}` or about {target}")),
                    ],
                ),
            );
        }
        FieldType::Text => {
            questions.insert(
                id.clone(),
                noul(format!("Does the request narrow down which {table} to return by the text of their `{name}`? Answer no if `{name}` is not mentioned.")),
            );
            questions.insert(
                format!("{id}.op"),
                choice(
                    format!(
                        "How does the request compare the `{name}` of the {table} against a value?"
                    ),
                    MATCHES,
                ),
            );
            questions.insert(
                format!("{id}.value"),
                value_choice(
                    format!(
                        "Which text does the request want the `{name}` of the {table} to match?"
                    ),
                    found.phrases.clone(),
                ),
            );
        }
        FieldType::Number | FieldType::Datetime => {}
    }
}

/// Whether "has no value" is a sensible question for this field. A yes/no or
/// fixed-choice field is asked about by its value instead.
fn can_be_empty(field: &Field) -> bool {
    !field.required && !matches!(field.kind, FieldType::Bool | FieldType::Enum { .. })
}

/// Reads answers and tracks the weakest one used, which becomes the
/// confidence of the whole query.
struct Reader<'a> {
    answers: &'a BTreeMap<String, Answer>,
    confidence: f64,
}

impl<'a> Reader<'a> {
    fn probability(&self, id: &str) -> f64 {
        match self.answers.get(id) {
            Some(Answer::Noul { noul }) => *noul,
            _ => 0.0,
        }
    }

    fn yes(&mut self, id: &str) -> bool {
        let probability = self.probability(id);
        self.confidence = self.confidence.min(probability.max(1.0 - probability));
        probability >= 0.5
    }

    /// Like [`Self::pick`], but does not count toward the confidence.
    fn peek(&self, id: &str) -> Option<&'a str> {
        match self.answers.get(id) {
            Some(Answer::Choice { choice, .. }) if choice != NONE => Some(choice.as_str()),
            _ => None,
        }
    }

    fn pick(&mut self, id: &str) -> Option<&'a str> {
        let Some(Answer::Choice { choice, confidence }) = self.answers.get(id) else {
            return None;
        };
        self.confidence = self.confidence.min(*confidence);
        (choice != NONE).then_some(choice.as_str())
    }
}

fn filter(field: &str, op: Op, value: Value) -> Filter {
    Filter {
        field: field.to_owned(),
        op,
        value,
    }
}

fn unclear(table: &str, part: &str) -> String {
    format!(
        "the request is about `{table}`, but {part} could not be worked out from it. Use find() with an explicit query."
    )
}

fn comparison(name: &str) -> Op {
    match name {
        "is_not" => Op::Ne,
        "more_than" => Op::Gt,
        "at_least" => Op::Gte,
        "less_than" => Op::Lt,
        "at_most" => Op::Lte,
        _ => Op::Eq,
    }
}

/// The filters a field gets from its own questions. Number and date fields
/// have none: they are filled from the numbers and the time period.
fn field_filters(reader: &mut Reader<'_>, table: &str, field: &Field) -> Built<Vec<Filter>> {
    let name = field.name.as_str();
    let id = format!("{table}.{name}");
    let mut filters = Vec::new();
    match &field.kind {
        FieldType::Enum { .. } => {
            if let Some(picked) = reader.pick(&id) {
                let (op, value) = picked
                    .strip_prefix("not_")
                    .map_or((Op::Eq, picked), |value| (Op::Ne, value));
                filters.push(filter(name, op, Value::from(value)));
            }
        }
        FieldType::Bool => {
            if let Some(picked) = reader.pick(&id) {
                filters.push(filter(name, Op::Eq, Value::Bool(picked == "yes")));
            }
        }
        FieldType::Ref { table: target } => {
            if reader.pick(&id) == Some("by_property") {
                return Err(format!(
                    "ask() filters `{table}` by their own fields only, and this request describes their `{name}` by a property of `{target}`. Find that `{target}` document first, then filter `{name}` by its id."
                ));
            }
        }
        FieldType::Text => {
            if reader.yes(&id) {
                let op = match reader.pick(&format!("{id}.op")) {
                    Some("is_not") => Op::Ne,
                    _ => Op::Contains,
                };
                let value = reader.pick(&format!("{id}.value")).ok_or_else(|| {
                    unclear(table, &format!("the text to match `{name}` against"))
                })?;
                filters.push(filter(name, op, Value::from(value)));
            }
        }
        FieldType::Number | FieldType::Datetime => {}
    }
    Ok(filters)
}

/// What the numbers in the request turned out to be.
#[derive(Default)]
struct Numbers {
    filters: Vec<Filter>,
    limit: Option<u32>,
    /// The count in "the last N days".
    span: Option<i64>,
}

fn read_numbers(reader: &mut Reader<'_>, def: &TableDef, found: &Candidates) -> Numbers {
    let mut numbers = Numbers::default();
    for (index, (_, value)) in found.numbers.iter().enumerate() {
        match reader.pick(&format!("{}.num.{index}", def.name)) {
            None => {}
            Some("limit") => numbers.limit = value.as_u64().and_then(|n| u32::try_from(n).ok()),
            Some("period") => numbers.span = value.as_i64(),
            Some(role) => {
                let is_ref = def
                    .field(role)
                    .is_some_and(|field| matches!(field.kind, FieldType::Ref { .. }));
                let op = match reader.pick(&format!("num.{index}.op")) {
                    Some("is_not") => Op::Ne,
                    Some(name) if !is_ref => comparison(name),
                    _ => Op::Eq,
                };
                numbers.filters.push(filter(role, op, value.clone()));
            }
        }
    }
    numbers
}

type Bounds = Vec<(Op, Date)>;

fn date_bounds(
    reader: &mut Reader<'_>,
    period: &str,
    span: Option<i64>,
    found: &Candidates,
    today: Date,
) -> Option<Bounds> {
    match period {
        "within_last_n" | "more_than_n_ago" => {
            let unit = reader.pick("period.unit").unwrap_or("days");
            let edge = back(today, span?, unit)?;
            let op = if period == "within_last_n" {
                Op::Gte
            } else {
                Op::Lt
            };
            Some(vec![(op, edge)])
        }
        "on_date" | "before_date" | "after_date" | "since_date" => {
            let date = found.date(reader.pick("period.date")?)?;
            match period {
                "on_date" => between(date, plus(date, 1)?),
                "before_date" => Some(vec![(Op::Lt, date)]),
                "after_date" => Some(vec![(Op::Gte, plus(date, 1)?)]),
                _ => Some(vec![(Op::Gte, date)]),
            }
        }
        _ => named_period(period, today),
    }
}

fn named_period(period: &str, today: Date) -> Option<Bounds> {
    let monday = plus(today, -i64::from(today.weekday().number_days_from_monday()))?;
    let (year, month) = (today.year(), today.month());
    match period {
        "today" => between(today, plus(today, 1)?),
        "yesterday" => between(plus(today, -1)?, today),
        "this_week" => between(monday, plus(monday, 7)?),
        "last_week" => between(plus(monday, -7)?, monday),
        "this_month" => month_bounds(year, month),
        "last_month" if month == Month::January => {
            month_bounds(year.checked_sub(1)?, Month::December)
        }
        "last_month" => month_bounds(year, month.previous()),
        "this_year" => year_bounds(year),
        "last_year" => year_bounds(year.checked_sub(1)?),
        _ => None,
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "every caller is an Option-returning match arm"
)]
fn between(start: Date, end: Date) -> Option<Bounds> {
    Some(vec![(Op::Gte, start), (Op::Lt, end)])
}

fn plus(date: Date, days: i64) -> Option<Date> {
    date.checked_add(Duration::days(days))
}

/// The date `count` units before `date`. A day that does not exist in the
/// target month, such as 31 February, becomes that month's last day.
fn back(date: Date, count: i64, unit: &str) -> Option<Date> {
    let months = match unit {
        "weeks" => return plus(date, count.checked_mul(-7)?),
        "months" => count,
        "years" => count.checked_mul(12)?,
        _ => return plus(date, count.checked_neg()?),
    };
    let index = i64::from(date.year())
        .checked_mul(12)?
        .checked_add(i64::from(u8::from(date.month())))?
        .checked_sub(1)?
        .checked_sub(months)?;
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = Month::try_from(u8::try_from(index.rem_euclid(12)).ok()?.checked_add(1)?).ok()?;
    (1..=date.day())
        .rev()
        .find_map(|day| Date::from_calendar_date(year, month, day).ok())
}

fn first_of(year: i32, month: Month) -> Option<Date> {
    Date::from_calendar_date(year, month, 1).ok()
}

fn month_bounds(year: i32, month: Month) -> Option<Bounds> {
    let next_year = if month == Month::December {
        year.checked_add(1)?
    } else {
        year
    };
    between(first_of(year, month)?, first_of(next_year, month.next())?)
}

fn year_bounds(year: i32) -> Option<Bounds> {
    between(
        first_of(year, Month::January)?,
        first_of(year.checked_add(1)?, Month::January)?,
    )
}

/// The request's time condition: the filters it becomes, and the words it
/// was understood as.
struct TimeCondition {
    field: String,
    phrase: String,
    filters: Vec<Filter>,
}

fn date_filters(
    reader: &mut Reader<'_>,
    table: &str,
    span: Option<i64>,
    found: &Candidates,
    today: Date,
) -> Built<Option<TimeCondition>> {
    let Some(period) = reader.pick("period") else {
        return Ok(None);
    };
    let field = reader
        .pick(&format!("{table}.date_field"))
        .ok_or_else(|| unclear(table, "which date the time condition applies to"))?;
    let bounds = date_bounds(reader, period, span, found, today)
        .ok_or_else(|| unclear(table, "the time period"))?;
    let unit = reader.peek("period.unit").unwrap_or("days");
    let date = reader.peek("period.date").unwrap_or_default();
    let count = span.unwrap_or_default();
    let phrase = match period {
        "within_last_n" => format!("within the last {count} {unit}"),
        "more_than_n_ago" => format!("more than {count} {unit} ago"),
        "on_date" => format!("on {date}"),
        "before_date" => format!("before {date}"),
        "after_date" => format!("after {date}"),
        "since_date" => format!("on or after {date}"),
        named => PERIODS
            .iter()
            .find(|(name, _)| *name == named)
            .map_or(named, |(_, description)| description)
            .to_owned(),
    };
    Ok(Some(TimeCondition {
        field: field.to_owned(),
        phrase,
        filters: bounds
            .into_iter()
            .map(|(op, date)| filter(field, op, Value::String(date.to_string())))
            .collect(),
    }))
}

/// A query together with its description in plain words.
struct Understood {
    query: Query,
    description: String,
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

fn interpret(
    def: &TableDef,
    reader: &mut Reader<'_>,
    found: &Candidates,
    today: Date,
) -> Built<Understood> {
    let table = &def.name;
    let mut query = Query::table(table);
    for field in &def.fields {
        query.filters.extend(field_filters(reader, table, field)?);
    }
    let numbers = read_numbers(reader, def, found);
    query.filters.extend(numbers.filters);
    query.limit = numbers.limit;
    for field in &def.fields {
        let by_id = matches!(field.kind, FieldType::Ref { .. })
            && reader.pick(&format!("{table}.{}", field.name)) == Some("by_id");
        if by_id
            && !query
                .filters
                .iter()
                .any(|filter| filter.field == field.name)
        {
            return Err(unclear(table, &format!("the id for `{}`", field.name)));
        }
    }
    let time = date_filters(reader, table, numbers.span, found, today)?;
    if let Some(time) = &time {
        query.filters.extend(time.filters.iter().cloned());
    }
    if let Some(field) = reader.pick(&format!("{table}.missing")) {
        query.filters.push(filter(field, Op::Eq, Value::Null));
    }
    // A field that already has a value filter is known to be set.
    let present = format!("{table}.present");
    if let Some(field) = reader.peek(&present)
        && !query.filters.iter().any(|filter| filter.field == field)
        && let Some(field) = reader.pick(&present)
    {
        query.filters.push(filter(field, Op::Ne, Value::Null));
    }
    if reader.yes("sort") {
        let field = reader
            .pick(&format!("{table}.sort.field"))
            .ok_or_else(|| unclear(table, "the field to order by"))?;
        query.sort = Some(Sort {
            field: field.to_owned(),
            descending: reader.pick("sort.dir") != Some("ascending"),
        });
    }
    let description = describe(&query, time.as_ref());
    Ok(Understood { query, description })
}

pub(crate) fn plan(defs: &[TableDef], text: &str, today: Date, judge: &dyn Judge) -> Result<Plan> {
    let found = Candidates::find(text);
    let mut questions = global_questions(defs, &found);
    for def in defs {
        table_questions(&mut questions, def, &found);
    }
    let judgement = judge.judge(text, &questions)?;
    let mut reader = Reader {
        answers: &judgement.answers,
        confidence: 1.0,
    };
    let mut usage = judgement.usage;
    let (query, refusal) = match decide(defs, &mut reader, &found, today) {
        Err(reason) => (None, Some(reason)),
        Ok(understood) if reader.confidence < MIN_CONFIDENCE => {
            let reason = format!(
                "not confident enough ({:.2}) to run this. Check the attached query and pass it to find() if it is right.",
                reader.confidence
            );
            (Some(understood.query), Some(reason))
        }
        Ok(understood) => {
            let check = judge.judge(text, &verification(&understood.description))?;
            usage.input_tokens += check.usage.input_tokens;
            usage.output_tokens += check.usage.output_tokens;
            let mut checker = Reader {
                answers: &check.answers,
                confidence: reader.confidence,
            };
            let refusal = checker.yes("leftover").then(|| {
                format!("the request seems to ask for something the query leaves out. It was understood as: {}. Check the attached query, and use find() if a condition is missing.", understood.description)
            });
            reader.confidence = checker.confidence;
            (Some(understood.query), refusal)
        }
    };
    Ok(Plan {
        query,
        confidence: reader.confidence,
        refusal,
        usage,
    })
}

/// A second, small request: does the query cover everything that was asked?
/// It catches a condition that was silently dropped.
fn verification(description: &str) -> Questions {
    Questions::from([(
        "leftover".to_owned(),
        noul(format!(
            "A search was built to answer the request. The search returns: {description}. Does the request ask for a condition, restriction or detail that this search leaves out?"
        )),
    )])
}

fn decide(
    defs: &[TableDef],
    reader: &mut Reader<'_>,
    found: &Candidates,
    today: Date,
) -> Built<Understood> {
    let unsupported = UNSUPPORTED
        .iter()
        .find(|(id, _)| reader.probability(id) >= 0.5);
    if let Some((_, reason)) = unsupported {
        return Err((*reason).to_owned());
    }
    let table = reader.pick("table");
    let Some(def) = defs.iter().find(|def| Some(def.name.as_str()) == table) else {
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        return Err(format!(
            "the request does not match any table. Tables: {}.",
            names.join(", ")
        ));
    };
    interpret(def, reader, found, today)
}
