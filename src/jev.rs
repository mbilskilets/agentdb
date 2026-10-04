use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::Agent;

use crate::error::{DbError, Result};

const URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
const KEY_VAR: &str = "TYPESAFE_API_KEY";
/// A request normally takes well under a second.
const TIMEOUT: Duration = Duration::from_secs(10);

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
    agent: Agent,
}

impl fmt::Debug for Jev {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Jev { key: <hidden> }")
    }
}

impl Jev {
    pub fn new(key: impl Into<String>) -> Self {
        Self::with_timeout(key, TIMEOUT)
    }

    fn with_timeout(key: impl Into<String>, timeout: Duration) -> Self {
        let agent = Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .into();
        Self {
            key: key.into(),
            agent,
        }
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

impl Jev {
    fn judge_at(
        &self,
        url: &str,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Judgement> {
        let request = Request {
            state,
            model: MODEL,
            questions,
        };
        self.agent
            .post(url)
            .header("Authorization", format!("Bearer {}", self.key))
            .send_json(&request)
            .and_then(|mut response| response.body_mut().read_json::<Judgement>())
            .map_err(|error| match error {
                ureq::Error::Timeout(_) => {
                    DbError::Jev("the model took too long to answer".to_owned())
                }
                other => DbError::Jev(other.to_string()),
            })
    }
}

impl Judge for Jev {
    fn judge(&self, state: &str, questions: &BTreeMap<String, Question>) -> Result<Judgement> {
        self.judge_at(URL, state, questions)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use super::Jev;

    #[test]
    fn a_model_that_never_answers_fails_instead_of_blocking() {
        let stalled = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", stalled.local_addr().unwrap());
        let jev = Jev::with_timeout("secret-key", Duration::from_millis(200));

        let started = Instant::now();
        let error = jev
            .judge_at(&url, "all clients", &BTreeMap::new())
            .unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(error.code(), "model_unavailable");
        assert_eq!(
            error.to_string(),
            "the language model request failed: the model took too long to answer. find() works without the model."
        );
    }

    #[test]
    fn the_key_is_hidden_from_debug_output_and_errors() {
        let jev = Jev::new("secret-key");
        assert_eq!(format!("{jev:?}"), "Jev { key: <hidden> }");

        let refused = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", refused.local_addr().unwrap());
        drop(refused);
        let error = jev
            .judge_at(&url, "all clients", &BTreeMap::new())
            .unwrap_err();
        assert!(!error.to_string().contains("secret-key"));
        assert!(!format!("{error:?}").contains("secret-key"));
    }
}
