//! Finds the values an English request mentions, for the model to pick from.

use serde_json::Value;
use time::Date;
use time::format_description::well_known::Iso8601;

pub(super) const MAX_PHRASE_WORDS: usize = 3;
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

/// Values found in the request text. The model picks among these because it
/// cannot produce a value of its own.
pub(super) struct Candidates {
    /// Each number, under the words the request wrote it with.
    pub numbers: Vec<(String, Value)>,
    pub dates: Vec<(String, Date)>,
    pub phrases: Vec<String>,
}

impl Candidates {
    pub(super) fn find(text: &str) -> Self {
        let tokens: Vec<&str> = text
            .split_whitespace()
            .filter(|token| token.chars().any(char::is_alphanumeric))
            .collect();
        let words: Vec<&str> = tokens
            .iter()
            .map(|token| token.trim_matches(|c: char| !c.is_alphanumeric()))
            .collect();
        let mut numbers: Vec<(String, Value)> = Vec::new();
        for (index, (token, word)) in tokens.iter().zip(&words).enumerate() {
            let next = words.get(index + 1).copied().unwrap_or_default();
            let found = written_number(token, word).or_else(|| counted_unit(word, next));
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

    pub(super) fn date(&self, label: &str) -> Option<Date> {
        let (_, date) = self.dates.iter().find(|(name, _)| name == label)?;
        Some(*date)
    }
}

/// A number written in digits. `token` is the word as the request wrote it,
/// so a minus sign in front of the digits is kept.
fn written_number(token: &str, word: &str) -> Option<(String, Value)> {
    let negative = token
        .chars()
        .take_while(|c| !c.is_alphanumeric())
        .any(|c| c == '-');
    let label = if negative {
        format!("-{word}")
    } else {
        word.to_owned()
    };
    let value = parse_number(&label)?;
    Some((label, value))
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
        if !found.contains(&phrase) && found.len() < MAX_PHRASES {
            found.push(phrase);
        }
    }
    found
}

/// Reads `1500`, `-20`, `1,500`, `5k` and `1.5m`.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Candidates;

    fn numbers(text: &str) -> Vec<(String, serde_json::Value)> {
        Candidates::find(text).numbers
    }

    #[test]
    fn numbers_are_read_in_every_written_form() {
        assert_eq!(
            numbers("over $1,500 or 5k, maybe 1.5m"),
            [
                ("1,500".to_owned(), json!(1500)),
                ("5k".to_owned(), json!(5000)),
                ("1.5m".to_owned(), json!(1_500_000.0)),
            ]
        );
    }

    #[test]
    fn a_minus_sign_in_front_of_a_number_is_kept() {
        assert_eq!(
            numbers("balance below -20 or (-$3.5)"),
            [
                ("-20".to_owned(), json!(-20)),
                ("-3.5".to_owned(), json!(-3.5)),
            ]
        );
    }

    #[test]
    fn a_dash_between_words_is_not_a_minus_sign() {
        assert_eq!(numbers("revenue - 20"), [("20".to_owned(), json!(20))]);
    }

    #[test]
    fn a_counted_unit_is_a_number() {
        assert_eq!(
            numbers("signed more than a year ago"),
            [("a year".to_owned(), json!(1))]
        );
    }

    #[test]
    fn phrases_are_runs_of_up_to_three_words() {
        assert_eq!(
            Candidates::find("clients named Acme Corp").phrases,
            [
                "clients",
                "named",
                "Acme",
                "Corp",
                "clients named",
                "named Acme",
                "Acme Corp",
                "clients named Acme",
                "named Acme Corp",
            ]
        );
    }
}
