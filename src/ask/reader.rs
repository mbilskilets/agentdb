//! Reads the model's answers to one request and tracks how far they can be
//! trusted.

use std::collections::BTreeMap;

use crate::jev::{Answer, Question};

use super::questions::{NONE, Questions};

/// Reads answers and lowers `confidence` to the weakest one used. An answer
/// that is missing, of the wrong kind, or not one of the options offered
/// leaves nothing to trust, so it lowers the confidence to zero.
pub(super) struct Reader<'a> {
    questions: &'a Questions,
    answers: &'a BTreeMap<String, Answer>,
    confidence: &'a mut f64,
}

impl<'a> Reader<'a> {
    pub(super) const fn new(
        questions: &'a Questions,
        answers: &'a BTreeMap<String, Answer>,
        confidence: &'a mut f64,
    ) -> Self {
        Self {
            questions,
            answers,
            confidence,
        }
    }

    /// The probability of a yes, without counting it toward the confidence.
    pub(super) fn probability(&mut self, id: &str) -> Option<f64> {
        let probability = match self.answers.get(id) {
            Some(Answer::Noul { noul }) if is_probability(*noul) => Some(*noul),
            _ => None,
        };
        self.usable(probability)
    }

    pub(super) fn yes(&mut self, id: &str) -> bool {
        let Some(probability) = self.probability(id) else {
            return false;
        };
        self.weaken(probability.max(1.0 - probability));
        probability >= 0.5
    }

    /// Like [`Self::pick`], but does not count toward the confidence.
    pub(super) fn peek(&mut self, id: &str) -> Option<&'a str> {
        let (option, _) = self.choice(id)?;
        (option != NONE).then_some(option)
    }

    /// The option picked for `id`, unless it is [`NONE`].
    pub(super) fn pick(&mut self, id: &str) -> Option<&'a str> {
        let (option, confidence) = self.choice(id)?;
        self.weaken(confidence);
        (option != NONE).then_some(option)
    }

    fn choice(&mut self, id: &str) -> Option<(&'a str, f64)> {
        let choice = self.offered_choice(id);
        self.usable(choice)
    }

    /// The model's pick and how sure it was, when the pick is one of the
    /// options the question offered.
    fn offered_choice(&self, id: &str) -> Option<(&'a str, f64)> {
        let Some(Question::Choice { criteria, .. }) = self.questions.get(id) else {
            return None;
        };
        let Some(Answer::Choice { choice, confidence }) = self.answers.get(id) else {
            return None;
        };
        let (option, _) = criteria.get_key_value(choice)?;
        is_probability(*confidence).then_some((option.as_str(), *confidence))
    }

    fn weaken(&mut self, certainty: f64) {
        *self.confidence = self.confidence.min(certainty);
    }

    fn usable<T>(&mut self, answer: Option<T>) -> Option<T> {
        if answer.is_none() {
            *self.confidence = 0.0;
        }
        answer
    }
}

fn is_probability(value: f64) -> bool {
    (0.0..=1.0).contains(&value)
}
