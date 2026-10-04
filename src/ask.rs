//! Turns an English request into a [`Query`].
//!
//! The model cannot write a query. This module lists every choice the schema
//! allows, asks the model to pick, and assembles the picks. It asks in three
//! small requests: which table, then everything about that table, then
//! whether the assembled query leaves anything out.

mod calendar;
mod candidates;
mod interpret;
mod questions;
mod reader;

use std::collections::BTreeMap;

use serde::Serialize;
use time::Date;

use crate::db::Page;
use crate::error::{DbError, Result};
use crate::jev::{Answer, Judge, Question, Usage};
use crate::query::Query;
use crate::schema::TableDef;

use candidates::Candidates;
use questions::{Questions, id};
use reader::Reader;

const MIN_CONFIDENCE: f64 = 0.6;
/// The model accepts 255 options per choice.
const MAX_OPTIONS: usize = 255;
/// The model accepts 64k tokens per request, and 32k for the request text
/// plus its longest question. Text of this size stays under both.
const MAX_REQUEST_BYTES: usize = 64_000;
const TOO_LARGE: &str = "ask() could not fit this request and the choices the schema allows into one request to the model. Shorten the request if it is long; otherwise use find() with an explicit query.";

/// The result of [`crate::AgentDb::ask`]. `page` is present only when the
/// query was run; otherwise `refusal` says why not.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Asked {
    /// How the request was understood. Present even when not run, so the
    /// caller can check it and pass it to `find()`.
    pub query: Option<Query>,
    /// The weakest of the model's answers that shaped the query, 0 to 1. A
    /// query runs only at 0.6 or above. The final check, whether the query
    /// leaves out part of the request, is a separate yes or no and does not
    /// lower this number.
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

/// Why a request stopped before it had a query to run.
enum Stop {
    /// `ask()` will not run this. The reason is worded for the caller.
    Refused(String),
    Failed(DbError),
}

impl From<String> for Stop {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}

impl From<DbError> for Stop {
    fn from(error: DbError) -> Self {
        Self::Failed(error)
    }
}

pub(crate) fn plan(defs: &[TableDef], text: &str, today: Date, judge: &dyn Judge) -> Result<Plan> {
    let mut plan = Plan {
        query: None,
        confidence: 1.0,
        refusal: None,
        usage: Usage::default(),
    };
    match understand(&mut plan, defs, text, today, judge) {
        Ok(()) => {}
        Err(Stop::Refused(reason)) => plan.refusal = Some(reason),
        Err(Stop::Failed(error)) => return Err(error),
    }
    Ok(plan)
}

/// Fills in `plan` one request at a time, stopping at the first reason not
/// to run it.
fn understand(
    plan: &mut Plan,
    defs: &[TableDef],
    text: &str,
    today: Date,
    judge: &dyn Judge,
) -> std::result::Result<(), Stop> {
    let found = Candidates::find(text);

    let routing = questions::routing(defs);
    let answers = consult(judge, text, &routing, &mut plan.usage)?;
    let mut reader = Reader::new(&routing, &answers, &mut plan.confidence);
    let def = interpret::route(defs, &mut reader)?;

    let about = questions::about(def, &found);
    let answers = consult(judge, text, &about, &mut plan.usage)?;
    let mut reader = Reader::new(&about, &answers, &mut plan.confidence);
    let understood = interpret::interpret(def, &mut reader, &found, today)?;
    plan.query = Some(understood.query);
    if plan.confidence < MIN_CONFIDENCE {
        return Err(unsure(plan.confidence));
    }

    let check = questions::verification(&understood.description);
    let answers = consult(judge, text, &check, &mut plan.usage)?;
    let mut reader = Reader::new(&check, &answers, &mut plan.confidence);
    match reader.probability(id::LEFTOVER) {
        Some(left_out) if left_out < 0.5 => Ok(()),
        Some(_) => Err(Stop::Refused(format!(
            "the request seems to ask for something the query leaves out. It was understood as: {}. Check the attached query, and use find() if a condition is missing.",
            understood.description
        ))),
        None => Err(unsure(plan.confidence)),
    }
}

fn unsure(confidence: f64) -> Stop {
    Stop::Refused(format!(
        "not confident enough ({confidence:.2}) to run this. Check the attached query and pass it to find() if it is right."
    ))
}

/// Sends one request to the model and adds what it cost to `usage`.
fn consult(
    judge: &dyn Judge,
    text: &str,
    questions: &Questions,
    usage: &mut Usage,
) -> std::result::Result<BTreeMap<String, Answer>, Stop> {
    if !fits(text, questions) {
        return Err(Stop::Refused(TOO_LARGE.to_owned()));
    }
    let judgement = judge.judge(text, questions)?;
    usage.input_tokens += judgement.usage.input_tokens;
    usage.output_tokens += judgement.usage.output_tokens;
    Ok(judgement.answers)
}

/// Whether the model will accept `questions` about `text`.
fn fits(text: &str, questions: &Questions) -> bool {
    let options_fit = questions.values().all(|question| match question {
        Question::Choice { criteria, .. } => criteria.len() <= MAX_OPTIONS,
        Question::Noul { .. } => true,
    });
    options_fit
        && serde_json::to_string(questions)
            .is_ok_and(|json| text.len().saturating_add(json.len()) <= MAX_REQUEST_BYTES)
}
