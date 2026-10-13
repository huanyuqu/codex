//! Close reading: a model reads blocks in full, numbered by line, and decides how much of each
//! a request needs. Recall reads the blocks whose cards were not enough to decide, and large
//! picked blocks that an excerpt may replace.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::history::History;
use crate::prompt::AnswerError;
use crate::prompt::ModelPrompt;
use crate::prompt::label_position;
use crate::prompt::parse_answer;
use crate::text::block_lines;
use crate::text::estimate_tokens;
use crate::text::truncate;
use crate::view::LineRange;
use crate::view::Presentation;

const INSTRUCTIONS: &str = include_str!("../prompts/read_instructions.md");

const MAX_REASON_CHARS: usize = 120;

/// Size limits of close reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
    /// Tokens of block text in one reading request; more blocks are split across requests.
    pub max_prompt_tokens: u32,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_prompt_tokens: 24_000,
        }
    }
}

/// A rendered reading request.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadPrompt {
    pub prompt: ModelPrompt,
    /// History position of the block each label `R1`, `R2`, … names.
    pub blocks: Vec<usize>,
    /// Number of lines of each block, for validating excerpts.
    pub line_counts: Vec<usize>,
}

/// Renders reading requests for `blocks`, splitting them so that each request stays within the
/// limits. A block larger than the limit gets a request of its own.
pub fn build_read_prompts(
    history: &History<'_>,
    question: &str,
    blocks: &[usize],
    limits: ReadLimits,
) -> Vec<ReadPrompt> {
    let mut prompts = Vec::new();
    let mut batch: Vec<(usize, Vec<String>)> = Vec::new();
    let mut batch_tokens = 0;
    for index in blocks {
        let lines = block_lines(history.block(*index).items);
        let tokens: u32 = lines.iter().map(|line| estimate_tokens(line) + 2).sum();
        if !batch.is_empty() && batch_tokens + tokens > limits.max_prompt_tokens {
            prompts.push(read_prompt(history, question, std::mem::take(&mut batch)));
            batch_tokens = 0;
        }
        batch.push((*index, lines));
        batch_tokens += tokens;
    }
    if !batch.is_empty() {
        prompts.push(read_prompt(history, question, batch));
    }
    prompts
}

fn read_prompt(
    history: &History<'_>,
    question: &str,
    batch: Vec<(usize, Vec<String>)>,
) -> ReadPrompt {
    let mut input = String::from("## Blocks\n");
    let mut blocks = Vec::new();
    let mut line_counts = Vec::new();
    for (label, (index, lines)) in batch.into_iter().enumerate() {
        let card = history.card(index);
        let place = match history.origin(index) {
            Some(branch) => format!("branch {branch}"),
            None => format!("turn {}", history.turn(index)),
        };
        input.push_str(&format!(
            "\n### R{} [{}] {place}: {}\n",
            label + 1,
            crate::render::kind_name(card.kind),
            card.title.trim()
        ));
        for (number, line) in lines.iter().enumerate() {
            input.push_str(&format!("{:>4}| {line}\n", number + 1));
        }
        blocks.push(index);
        line_counts.push(lines.len());
    }
    input.push_str("\n## Request\n");
    input.push_str(question.trim());
    input.push('\n');
    ReadPrompt {
        prompt: ModelPrompt {
            instructions: INSTRUCTIONS.to_string(),
            input,
            schema_name: "context_reading",
            schema: answer_schema(),
        },
        blocks,
        line_counts,
    }
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["blocks"],
        "properties": {
            "blocks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["block", "decision", "lines", "reason"],
                    "properties": {
                        "block": { "type": "string" },
                        "decision": {
                            "type": "string",
                            "enum": ["full", "excerpt", "summary", "drop"]
                        },
                        "lines": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["from", "to"],
                                "properties": {
                                    "from": { "type": "integer" },
                                    "to": { "type": "integer" }
                                }
                            }
                        },
                        "reason": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// What reading decided for one block: how to show it, or `None` to leave it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDecision {
    /// History position of the block.
    pub index: usize,
    pub presentation: Option<Presentation>,
    pub note: Option<String>,
}

/// The decisions of a reading answer, and the blocks it did not decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAnswer {
    pub decisions: Vec<ReadDecision>,
    pub missing: Vec<usize>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    blocks: Vec<AnswerBlock>,
}

#[derive(Deserialize)]
struct AnswerBlock {
    block: String,
    #[serde(default)]
    decision: String,
    #[serde(default)]
    lines: Vec<AnswerRange>,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct AnswerRange {
    from: i64,
    to: i64,
}

/// Parses the model's answer to `prompt`. An excerpt without valid lines becomes a summary.
pub fn parse_read_answer(prompt: &ReadPrompt, text: &str) -> Result<ReadAnswer, AnswerError> {
    let answer: Answer = parse_answer(text)?;
    let mut decided = HashSet::new();
    let mut decisions = Vec::new();
    for block in &answer.blocks {
        let Some(position) = label_position(&block.block, 'R', prompt.blocks.len()) else {
            continue;
        };
        if !decided.insert(position) {
            continue;
        }
        let presentation = match block.decision.trim() {
            "full" => Some(Presentation::Full),
            "summary" => Some(Presentation::Summary),
            "drop" => None,
            _ => {
                let lines = valid_ranges(&block.lines, prompt.line_counts[position]);
                if lines.is_empty() {
                    Some(Presentation::Summary)
                } else {
                    Some(Presentation::Excerpt { lines })
                }
            }
        };
        let reason = block.reason.trim();
        decisions.push(ReadDecision {
            index: prompt.blocks[position],
            presentation,
            note: (!reason.is_empty()).then(|| truncate(reason, MAX_REASON_CHARS)),
        });
    }
    let missing = (0..prompt.blocks.len())
        .filter(|position| !decided.contains(position))
        .map(|position| prompt.blocks[position])
        .collect();
    Ok(ReadAnswer { decisions, missing })
}

/// Ranges within `1..=line_count`, sorted, with overlapping and adjacent ranges merged.
fn valid_ranges(ranges: &[AnswerRange], line_count: usize) -> Vec<LineRange> {
    let last = i64::try_from(line_count).unwrap_or(i64::MAX);
    let mut valid: Vec<(i64, i64)> = ranges
        .iter()
        .map(|range| (range.from.max(1), range.to.min(last)))
        .filter(|(from, to)| from <= to)
        .collect();
    valid.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (from, to) in valid {
        match merged.last_mut() {
            Some(last) if from <= last.1 + 1 => last.1 = last.1.max(to),
            _ => merged.push((from, to)),
        }
    }
    merged
        .into_iter()
        .filter_map(|(from, to)| {
            Some(LineRange {
                from: u32::try_from(from).ok()?,
                to: u32::try_from(to).ok()?,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
