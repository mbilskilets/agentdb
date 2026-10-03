use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};

const URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
const KEY_VAR: &str = "TYPESAFE_API_KEY";

/// A question for the model. It never writes text: it picks one option or
/// gives the probability of "yes".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    Choice {
        instructions: String,
        /// Option name to its description.
        criteria: BTreeMap<String, String>,
    },
    Noul {
        instructions: String,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
    },
    Noul {
        noul: f64,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Judgement {
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

/// Answers a batch of questions about one piece of text. Implemented by
/// [`Jev`]; tests can supply their own.
pub trait Judge {
    /// # Errors
    /// Fails when the model cannot be reached or rejects the request.
    fn judge(&self, state: &str, questions: &BTreeMap<String, Question>) -> Result<Judgement>;
}

/// Client for the `TypeSafe` Jev model.
#[derive(Clone)]
pub struct Jev {
    key: String,
}

impl fmt::Debug for Jev {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Jev { key: <hidden> }")
    }
}

impl Jev {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }

    /// Reads the key from the `TYPESAFE_API_KEY` environment variable.
    ///
    /// # Errors
    /// [`DbError::MissingApiKey`] when the variable is unset or empty.
    pub fn from_env() -> Result<Self> {
        match std::env::var(KEY_VAR) {
            Ok(key) if !key.is_empty() => Ok(Self::new(key)),
            _ => Err(DbError::MissingApiKey),
        }
    }
}

#[derive(Serialize)]
struct Request<'a> {
    state: &'a str,
    model: &'static str,
    questions: &'a BTreeMap<String, Question>,
}

impl Judge for Jev {
    fn judge(&self, state: &str, questions: &BTreeMap<String, Question>) -> Result<Judgement> {
        let request = Request {
            state,
            model: MODEL,
            questions,
        };
        ureq::post(URL)
            .header("Authorization", format!("Bearer {}", self.key))
            .send_json(&request)
            .and_then(|mut response| response.body_mut().read_json::<Judgement>())
            .map_err(|error| DbError::Jev(error.to_string()))
    }
}
