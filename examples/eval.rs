//! Scores `ask()` against the graded tasks in `evals/cases.json`.
//!
//! Run with `./eval.sh`. Each task is an English request plus either the
//! query it should become or `"refuse"`. A task passes when `ask()` returns
//! the same documents as the expected query, or refuses when it should.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::time::Instant;

use agentdb::{AgentDb, Answer, Asked, Jev, Judge, Judgement, Query, Question, TableDef};
use serde::Deserialize;
use serde_json::Value;

/// Dollars per input token for Jev.
const PRICE: f64 = 0.042 / 1e6;
const LEVELS: [&str; 4] = ["simple", "normal", "hard", "superhard"];

#[derive(Deserialize)]
struct Fixture {
    now: String,
    tables: Vec<TableDef>,
    docs: Vec<Seed>,
}

#[derive(Deserialize)]
struct Seed {
    table: String,
    at: String,
    doc: Value,
}

#[derive(Deserialize)]
struct Case {
    level: String,
    ask: String,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Expect {
    Query(Query),
    Refuse(Refuse),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Refuse {
    Refuse,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Right documents, or a correct refusal.
    Pass,
    /// Ran a query that returned the wrong documents, or ran when it should
    /// have refused. The dangerous outcome.
    Wrong,
    /// Refused a request it should have answered. Safe but unhelpful.
    Missed,
}

impl Verdict {
    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS  ",
            Self::Wrong => "WRONG ",
            Self::Missed => "MISSED",
        }
    }
}

#[derive(Default)]
struct Tally {
    pass: u32,
    wrong: u32,
    missed: u32,
}

impl Tally {
    const fn add(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Pass => self.pass += 1,
            Verdict::Wrong => self.wrong += 1,
            Verdict::Missed => self.missed += 1,
        }
    }

    const fn total(&self) -> u32 {
        self.pass + self.wrong + self.missed
    }
}

/// Keeps the model's raw answers to every request of one task, so a failed
/// task can show them.
struct Recorder {
    jev: Jev,
    answers: RefCell<BTreeMap<String, Answer>>,
}

impl Judge for Recorder {
    fn judge(
        &self,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> agentdb::Result<Judgement> {
        let judgement = self.jev.judge(state, questions)?;
        self.answers.borrow_mut().extend(judgement.answers.clone());
        Ok(judgement)
    }
}

/// The answers that shaped the query, plus the ones the model was unsure of.
fn trace(answers: &BTreeMap<String, Answer>) -> String {
    let parts: Vec<String> = answers
        .iter()
        .filter_map(|(id, answer)| match answer {
            Answer::Noul { noul } if *noul >= 0.35 => Some(format!("{id}={noul:.2}")),
            Answer::Choice { choice, confidence }
                if !choice.starts_with("none") || *confidence < 0.7 =>
            {
                Some(format!("{id}={choice}({confidence:.2})"))
            }
            _ => None,
        })
        .collect();
    parts.join("  ")
}

fn seeded() -> Result<AgentDb, Box<dyn Error>> {
    let fixture: Fixture = serde_json::from_str(&fs::read_to_string("evals/fixture.json")?)?;
    let db = AgentDb::open_in_memory()?;
    for table in &fixture.tables {
        db.define_table(table)?;
    }
    for seed in fixture.docs {
        db.freeze_time(Some(&seed.at))?;
        db.insert(&seed.table, seed.doc)?;
    }
    db.freeze_time(Some(&fixture.now))?;
    Ok(db)
}

fn ids(db: &AgentDb, query: &Query) -> Result<Vec<i64>, Box<dyn Error>> {
    let mut ids: Vec<i64> = db.find(query)?.docs.iter().map(|doc| doc.id).collect();
    if query.sort.is_none() && query.limit.is_none() {
        ids.sort_unstable();
    }
    Ok(ids)
}

fn grade(db: &AgentDb, expect: &Expect, asked: &Asked) -> Result<Verdict, Box<dyn Error>> {
    let ran = asked.page.is_some();
    let Expect::Query(expected) = expect else {
        return Ok(if ran { Verdict::Wrong } else { Verdict::Pass });
    };
    let Some(page) = &asked.page else {
        return Ok(Verdict::Missed);
    };
    let mut got: Vec<i64> = page.docs.iter().map(|doc| doc.id).collect();
    if expected.sort.is_none() && expected.limit.is_none() {
        got.sort_unstable();
    }
    Ok(if got == ids(db, expected)? {
        Verdict::Pass
    } else {
        Verdict::Wrong
    })
}

fn explain(asked: &Asked) -> String {
    let query = asked
        .query
        .as_ref()
        .and_then(|query| serde_json::to_string(query).ok())
        .unwrap_or_else(|| "no query".to_owned());
    match &asked.refusal {
        Some(reason) => format!("refused: {reason}\n           understood as: {query}"),
        None => format!("ran: {query}"),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let db = seeded()?;
    let jev = Recorder {
        jev: Jev::from_env()?,
        answers: RefCell::new(BTreeMap::new()),
    };
    let verbose = std::env::args().any(|arg| arg == "--verbose");
    let path = std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with("--"))
        .unwrap_or_else(|| "evals/cases.json".to_owned());
    let cases: Vec<Case> = serde_json::from_str(&fs::read_to_string(path)?)?;

    let mut levels: BTreeMap<String, Tally> = BTreeMap::new();
    let mut millis: Vec<u128> = Vec::new();
    let mut tokens: u64 = 0;
    for case in &cases {
        jev.answers.borrow_mut().clear();
        let started = Instant::now();
        let asked = db.ask(&jev, &case.ask)?;
        millis.push(started.elapsed().as_millis());
        tokens += asked.usage.input_tokens;
        let verdict = grade(&db, &case.expect, &asked)?;
        levels.entry(case.level.clone()).or_default().add(verdict);
        println!(
            "{} {:<9} {:.2}  {}",
            verdict.label(),
            case.level,
            asked.confidence,
            case.ask
        );
        if verbose || verdict != Verdict::Pass {
            println!("           {}", explain(&asked));
            println!("           model: {}", trace(&jev.answers.borrow()));
        }
    }

    println!("\nlevel      pass  wrong  missed");
    let (mut pass, mut wrong, mut total) = (0, 0, 0);
    for level in LEVELS {
        let Some(tally) = levels.get(level) else {
            continue;
        };
        println!(
            "{level:<9} {:>2}/{:<2} {:>5} {:>7}",
            tally.pass,
            tally.total(),
            tally.wrong,
            tally.missed
        );
        pass += tally.pass;
        wrong += tally.wrong;
        total += tally.total();
    }
    millis.sort_unstable();
    let median = millis.get(millis.len() / 2).copied().unwrap_or_default();
    let slowest = millis.last().copied().unwrap_or_default();
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts are far below 2^52"
    )]
    let cost = tokens as f64 * PRICE;
    let per_ask = tokens / u64::from(total.max(1));
    println!(
        "\noverall {pass}/{total} pass, {wrong} wrong | median {median} ms, slowest {slowest} ms | {tokens} tokens ({per_ask} per ask), ${cost:.4}"
    );
    Ok(())
}
