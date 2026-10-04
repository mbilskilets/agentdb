//! The questions the model is asked, and the names its answers come back
//! under.
//!
//! Tables and fields are named with lowercase letters, digits and
//! underscores. Every option that is not such a name contains a space, and
//! every question id that carries a field name starts with `field.`, so no
//! schema can make two options or two questions share a name.

use std::collections::BTreeMap;

use crate::jev::Question;
use crate::query::Op;
use crate::schema::{Field, FieldType, TableDef};

use super::calendar::{PERIODS, UNITS};
use super::candidates::Candidates;

pub(super) type Questions = BTreeMap<String, Question>;

pub(super) mod id {
    pub(in crate::ask) const TABLE: &str = "table";
    pub(in crate::ask) const SORT: &str = "sort";
    pub(in crate::ask) const SORT_FIELD: &str = "sort.field";
    pub(in crate::ask) const SORT_DIRECTION: &str = "sort.direction";
    pub(in crate::ask) const PERIOD: &str = "period";
    pub(in crate::ask) const PERIOD_FIELD: &str = "period.field";
    pub(in crate::ask) const PERIOD_UNIT: &str = "period.unit";
    pub(in crate::ask) const PERIOD_DATE: &str = "period.date";
    pub(in crate::ask) const MISSING: &str = "missing";
    pub(in crate::ask) const PRESENT: &str = "present";
    pub(in crate::ask) const LEFTOVER: &str = "leftover";

    /// What the number at `index` in the request is for.
    pub(in crate::ask) fn number_role(index: usize) -> String {
        format!("number.{index}.role")
    }

    pub(in crate::ask) fn number_comparison(index: usize) -> String {
        format!("number.{index}.comparison")
    }

    /// How the request restricts the field called `name`.
    pub(in crate::ask) fn field(name: &str) -> String {
        format!("field.{name}")
    }

    pub(in crate::ask) fn text_match(name: &str) -> String {
        format!("field.{name}.match")
    }

    pub(in crate::ask) fn text_value(name: &str) -> String {
        format!("field.{name}.value")
    }
}

/// Says that no other option fits. Its four words keep it apart from every
/// name and from every phrase of the request, which has at most three.
pub(super) const NONE: &str = "none of the above";
pub(super) const YES: &str = "yes";
pub(super) const BY_ID: &str = "by_id";
pub(super) const BY_PROPERTY: &str = "by_property";
pub(super) const IS_NOT: &str = "is_not";
pub(super) const ASCENDING: &str = "ascending";
/// The number is the id of the documents themselves. `id` is reserved, so no
/// field has this name.
pub(super) const ROLE_ID: &str = "id";
pub(super) const ROLE_LIMIT: &str = "limit of results";
pub(super) const ROLE_SPAN: &str = "period of time";

/// Requests `ask()` does not answer: the question that detects each one, and
/// what to tell the caller.
pub(super) const UNSUPPORTED: [(&str, &str, &str); 4] = [
    (
        "write",
        "Does the request ask to create, change, or delete records, rather than only look them up?",
        "ask() only reads. This looks like a request to change data: use insert(), update() or delete().",
    ),
    (
        "statistic",
        "Does the request ask for a calculated figure such as a sum, an average or a total amount? Answer no if it asks for records, for a ranking such as the cheapest or the top 5, or for how many records there are.",
        "ask() returns documents and how many matched, not sums or averages. Fetch the documents and calculate from them.",
    ),
    (
        "either_or",
        "Does the request accept records that meet one condition OR a different condition, as alternatives? Answer no if every condition must hold together.",
        "ask() cannot combine conditions with OR. Call ask() once per alternative.",
    ),
    (
        "relational",
        "Does the request compare one record's value with another record's value or with a group average, as in \"earn more than their manager\" or \"above average\"? Answer no for sorting, ranking, top N, cheapest or biggest, and for comparisons with a fixed number.",
        "ask() cannot compare documents with each other. Fetch the documents and compare them yourself.",
    ),
];

const COMPARISONS: [(&str, &str, Op); 6] = [
    ("is", "exactly equal to the number", Op::Eq),
    (IS_NOT, "anything other than the number", Op::Ne),
    (
        "more_than",
        "strictly greater than the number: over, above, more than",
        Op::Gt,
    ),
    (
        "at_least",
        "the number or greater: at least, minimum, or more, from, between ... and",
        Op::Gte,
    ),
    (
        "less_than",
        "strictly less than the number: under, below, less than",
        Op::Lt,
    ),
    (
        "at_most",
        "the number or smaller: at most, maximum, up to, or less, between ... and",
        Op::Lte,
    ),
];
const MATCHES: [(&str, &str); 2] = [
    ("is", "it is, matches, or is named the value"),
    (
        IS_NOT,
        "it is anything other than the value: not, non-, except",
    ),
];

/// The operator a picked comparison stands for.
pub(super) fn comparison(picked: &str) -> Option<Op> {
    let (_, _, op) = COMPARISONS.iter().find(|(name, _, _)| *name == picked)?;
    Some(*op)
}

/// Every way a request can restrict an enum field: the option's name, and
/// the filter it stands for. Values can be any text, so each is wrapped in
/// words that keep the two kinds of option apart from each other and from
/// [`NONE`].
pub(super) fn enum_options(values: &[String]) -> impl Iterator<Item = (String, Op, &str)> {
    values.iter().flat_map(|value| {
        [
            (format!("is {value}"), Op::Eq, value.as_str()),
            (format!("not {value}"), Op::Ne, value.as_str()),
        ]
    })
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

/// The first request: which table the request is about, and whether it is
/// something `ask()` answers at all.
pub(super) fn routing(defs: &[TableDef]) -> Questions {
    let tables = defs
        .iter()
        .map(|def| (def.name.clone(), describe_table(def)))
        .chain([(
            NONE.to_owned(),
            "the request is not about any of these record types".to_owned(),
        )]);
    let mut questions = Questions::from([(
        id::TABLE.to_owned(),
        choice(
            "Which kind of record does the request ask for? A request may name records by one of their listed values, such as a status or a role.",
            tables,
        ),
    )]);
    for (id, instructions, _) in UNSUPPORTED {
        questions.insert(id.to_owned(), noul(instructions));
    }
    questions
}

/// The second request: everything the request can say about one table.
pub(super) fn about(def: &TableDef, found: &Candidates) -> Questions {
    let table = &def.name;
    let mut questions = sort_questions(def);
    questions.extend(period_questions(def, found));
    for field in &def.fields {
        questions.extend(field_questions(table, field, found));
    }
    for (index, (label, _)) in found.numbers.iter().enumerate() {
        questions.insert(id::number_role(index), number_role(def, label));
        questions.insert(
            id::number_comparison(index),
            choice(
                format!("The request mentions \"{label}\". How does it compare something against that number?"),
                COMPARISONS.map(|(name, description, _)| (name, description)),
            ),
        );
    }
    let nullable = || def.fields.iter().filter(|field| can_be_empty(field));
    questions.insert(
        id::MISSING.to_owned(),
        choice(
            format!("Does the request ask only for {table} that lack something, with words like without, no, never, missing or not yet? If so, what do they lack?"),
            nullable()
                .map(|field| (field.name.clone(), format!("only {table} that have no `{}`", field.name)))
                .chain([(NONE.to_owned(), "the request does not ask for records that lack something".to_owned())]),
        ),
    );
    questions.insert(
        id::PRESENT.to_owned(),
        choice(
            format!("Does the request say outright that the {table} must have something, whatever its value, with words like \"that have\", \"with a\" or \"having\"? If so, what must they have?"),
            nullable()
                .map(|field| (field.name.clone(), format!("only {table} that have some `{}`, whatever it is", field.name)))
                .chain([(NONE.to_owned(), "the request does not say this, which is the usual case".to_owned())]),
        ),
    );
    questions
}

/// The last request: does the query cover everything that was asked? It
/// catches a condition that was silently dropped.
pub(super) fn verification(description: &str) -> Questions {
    Questions::from([(
        id::LEFTOVER.to_owned(),
        noul(format!(
            "A search was built to answer the request. The search returns: {description}. Does the request ask for a condition, restriction or detail that this search leaves out?"
        )),
    )])
}

fn sort_questions(def: &TableDef) -> Questions {
    let table = &def.name;
    let field_names = def.fields.iter().map(|field| field.name.as_str());
    Questions::from([
        (
            id::SORT.to_owned(),
            noul(
                "Does the request explicitly ask for the results to be ordered or ranked, with words like sorted, ordered, top, highest, lowest, biggest, cheapest, newest, oldest, most or least?",
            ),
        ),
        (
            id::SORT_DIRECTION.to_owned(),
            choice(
                "In which direction does the request order the results?",
                [
                    (
                        "descending",
                        "largest, highest, newest or most recent first",
                    ),
                    (
                        ASCENDING,
                        "smallest, lowest, cheapest, oldest or earliest first",
                    ),
                ],
            ),
        ),
        (
            id::SORT_FIELD.to_owned(),
            choice(
                format!("Which property of the {table} does the request order or rank them by?"),
                field_names
                    .chain(["id", "created_at", "updated_at"])
                    .map(|name| (name.to_owned(), format!("ordered by `{name}`")))
                    .chain([(NONE.to_owned(), "no ordering is requested".to_owned())]),
            ),
        ),
    ])
}

fn period_questions(def: &TableDef, found: &Candidates) -> Questions {
    let table = &def.name;
    let date_columns = def
        .fields
        .iter()
        .filter(|field| field.kind == FieldType::Datetime)
        .map(|field| field.name.as_str())
        .chain(["created_at", "updated_at"]);
    Questions::from([
        (
            id::PERIOD.to_owned(),
            choice(
                "Which time period does the request restrict the records to?",
                PERIODS
                    .iter()
                    .copied()
                    .chain([(NONE, "the request has no time condition")]),
            ),
        ),
        (
            id::PERIOD_UNIT.to_owned(),
            choice(
                "Which unit of time does the request count in?",
                UNITS.map(|unit| (unit, unit)),
            ),
        ),
        (
            id::PERIOD_DATE.to_owned(),
            value_choice(
                "Which calendar date does the request's time condition use?".to_owned(),
                found.dates.iter().map(|(label, _)| label.clone()),
            ),
        ),
        (
            id::PERIOD_FIELD.to_owned(),
            choice(
                format!("Which date of the {table} does the request's time condition apply to?"),
                date_columns
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
        ),
    ])
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
        (ROLE_ID, format!("it is the id of the {table} themselves")),
        (
            ROLE_LIMIT,
            "it is how many results to return, as in top 5 or first 10".to_owned(),
        ),
        (
            ROLE_SPAN,
            "it counts days, weeks, months or years in a time span".to_owned(),
        ),
        (NONE, "it is something else".to_owned()),
    ];
    choice(
        format!("The request mentions \"{label}\". What is that number?"),
        fields.chain(others.map(|(name, description)| (name.to_owned(), description))),
    )
}

/// The questions about one field's own value. Number and date fields have
/// none: they are filled from the numbers and the time period.
fn field_questions(table: &str, field: &Field, found: &Candidates) -> Questions {
    let name = &field.name;
    let id = id::field(name);
    match &field.kind {
        FieldType::Enum { values } => {
            let restricted = enum_options(values).map(|(option, op, value)| {
                let description = match op {
                    Op::Ne => format!("only {table} whose `{name}` is anything except {value}"),
                    _ => format!("only {table} whose `{name}` is {value}"),
                };
                (option, description)
            });
            let unrestricted = (
                NONE.to_owned(),
                format!("the request does not restrict `{name}`"),
            );
            Questions::from([(
                id,
                choice(
                    format!("Which `{name}` does the request restrict the {table} to?"),
                    restricted.chain([unrestricted]),
                ),
            )])
        }
        FieldType::Bool => Questions::from([(
            id,
            choice(
                format!("Does the request restrict the {table} by whether they are `{name}`?"),
                [
                    (YES, format!("only {table} that are `{name}`")),
                    (
                        "no",
                        format!("only {table} that are not `{name}`: non-{name}, not {name}"),
                    ),
                    (NONE, format!("the request does not mention `{name}`")),
                ],
            ),
        )]),
        FieldType::Ref { table: target } => Questions::from([(
            id,
            choice(
                format!(
                    "Does the request restrict the {table} by their `{name}`, which links each of them to one of the {target}?"
                ),
                [
                    (BY_ID, format!("yes, by a numeric id, such as \"{name} 7\"")),
                    (
                        BY_PROPERTY,
                        format!(
                            "yes, the request talks about the `{name}` or the {target} of the {table} and describes it by name or by another property"
                        ),
                    ),
                    (
                        NONE,
                        format!("no, the request does not talk about a `{name}` or about {target}"),
                    ),
                ],
            ),
        )]),
        FieldType::Text => Questions::from([
            (
                id,
                noul(format!(
                    "Does the request narrow down which {table} to return by the text of their `{name}`? Answer no if `{name}` is not mentioned."
                )),
            ),
            (
                id::text_match(name),
                choice(
                    format!(
                        "How does the request compare the `{name}` of the {table} against a value?"
                    ),
                    MATCHES,
                ),
            ),
            (
                id::text_value(name),
                value_choice(
                    format!(
                        "Which text does the request want the `{name}` of the {table} to match?"
                    ),
                    found.phrases.clone(),
                ),
            ),
        ]),
        FieldType::Number | FieldType::Datetime => Questions::new(),
    }
}

/// Whether "has no value" is a sensible question for this field. A yes/no or
/// fixed-choice field is asked about by its value instead.
fn can_be_empty(field: &Field) -> bool {
    !field.required && !matches!(field.kind, FieldType::Bool | FieldType::Enum { .. })
}

#[cfg(test)]
mod tests {
    use super::{NONE, enum_options};
    use crate::ask::candidates::Candidates;

    #[test]
    fn no_phrase_of_a_request_can_be_mistaken_for_none() {
        let phrases = Candidates::find(&format!("clients named {NONE} please")).phrases;
        assert!(!phrases.iter().any(|phrase| phrase == NONE));
    }

    #[test]
    fn enum_values_that_look_like_other_options_stay_distinct() {
        let values = ["started", "not started", "is started", NONE].map(str::to_owned);
        let mut names: Vec<String> = enum_options(&values).map(|(name, _, _)| name).collect();
        names.push(NONE.to_owned());
        let offered = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), offered);
    }
}
