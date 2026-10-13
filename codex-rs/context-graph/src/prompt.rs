//! Requests to the shadow agent's models, and parsing of their JSON answers.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::text::truncate;

/// A request for a structured answer: fixed instructions, the input, and the JSON schema the
/// answer must follow.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPrompt {
    pub instructions: String,
    pub input: String,
    /// Name of the answer format, such as `index_cards`.
    pub schema_name: &'static str,
    pub schema: Value,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("answer is not valid JSON ({error}): {start:?}")]
pub struct AnswerError {
    pub error: String,
    pub start: String,
}

/// Parses a JSON answer, accepting one wrapped in a Markdown code fence.
pub fn parse_answer<T: DeserializeOwned>(text: &str) -> Result<T, AnswerError> {
    let json = strip_code_fence(text);
    serde_json::from_str(json).map_err(|err| AnswerError {
        error: err.to_string(),
        start: truncate(json, 80),
    })
}

pub(crate) fn strip_code_fence(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

/// Position of `label` in a list named `prefix1`, `prefix2`, …; a bare number is accepted too.
pub(crate) fn label_position(label: &str, prefix: char, len: usize) -> Option<usize> {
    let label = label.trim();
    let number = label
        .strip_prefix(prefix)
        .or_else(|| label.strip_prefix(prefix.to_ascii_lowercase()))
        .unwrap_or(label);
    let number: usize = number.parse().ok()?;
    (1..=len).contains(&number).then(|| number - 1)
}
