//! Pointwise screening: each card is judged on its own, as the probability that a request needs
//! its block. A model can judge a batch of cards per request, or a reranker can score them; either
//! way the cards of one batch are independent of each other, so batches run in parallel and very
//! long histories need no catalog budget.

use serde::Deserialize;
use serde_json::json;

use crate::history::History;
use crate::prompt::AnswerError;
use crate::prompt::ModelPrompt;
use crate::prompt::label_position;
use crate::prompt::parse_answer;
use crate::screen::ScreenLimits;
use crate::screen::card_lines;
use crate::view::Pick;
use crate::view::Presentation;

const INSTRUCTIONS: &str = include_str!("../prompts/score_instructions.md");

/// Scores at or above this pick a block in full.
pub const FULL_SCORE: f32 = 0.75;
/// Scores at or above this pick a block's summary.
pub const PICK_SCORE: f32 = 0.5;
/// Scores at or above this, below [`PICK_SCORE`], leave a block for reading.
pub const UNCERTAIN_SCORE: f32 = 0.3;

/// The cards a pointwise screening judges: the recallable blocks before the recent turns.
pub fn score_candidates(history: &History<'_>, recent_start: usize) -> Vec<usize> {
    (0..history.len())
        .filter(|index| history.recallable(*index) && !history.is_recent(*index, recent_start))
        .collect()
}

/// The text of each candidate's card, as a reranker reads it.
pub fn candidate_documents(history: &History<'_>, candidates: &[usize]) -> Vec<String> {
    candidates
        .iter()
        .map(|index| card_lines(history.card(*index), ScreenLimits::default()))
        .collect()
}

/// The prompt that has a model judge `documents`, labelled `C1`, `C2`, … within the batch.
pub fn score_prompt(question: &str, documents: &[String]) -> ModelPrompt {
    let mut input = String::from("## Cards\n");
    for (label, document) in documents.iter().enumerate() {
        input.push_str(&format!("C{} {document}\n", label + 1));
    }
    input.push_str("\n## Request\n");
    input.push_str(question.trim());
    input.push('\n');
    ModelPrompt {
        instructions: INSTRUCTIONS.to_string(),
        input,
        schema_name: "card_scores",
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["scores"],
            "properties": {
                "scores": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["card", "needed"],
                        "properties": {
                            "card": { "type": "string" },
                            "needed": { "type": "number" }
                        }
                    }
                }
            }
        }),
    }
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    scores: Vec<Score>,
}

#[derive(Deserialize)]
struct Score {
    card: String,
    #[serde(default)]
    needed: f32,
}

/// The score of each of `len` cards from a model's answer; cards it left out score 0.
pub fn parse_scores(text: &str, len: usize) -> Result<Vec<f32>, AnswerError> {
    let answer: Answer = parse_answer(text)?;
    let mut scores = vec![0.0; len];
    for score in answer.scores {
        if let Some(position) = label_position(&score.card, 'C', len) {
            scores[position] = score.needed.clamp(0.0, 1.0);
        }
    }
    Ok(scores)
}

/// Picks and blocks to read from the scores of `candidates`: the best first, at most
/// `max_uncertain` left for reading.
pub fn picks_from_scores(
    candidates: &[usize],
    scores: &[f32],
    max_uncertain: usize,
) -> (Vec<Pick>, Vec<usize>) {
    let mut ranked: Vec<(usize, f32)> = candidates
        .iter()
        .copied()
        .zip(scores.iter().copied())
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut picks = Vec::new();
    let mut uncertain = Vec::new();
    for (index, score) in ranked {
        if score >= PICK_SCORE {
            picks.push(Pick {
                index,
                presentation: if score >= FULL_SCORE {
                    Presentation::Full
                } else {
                    Presentation::Summary
                },
                note: Some(format!("scored {score:.2}")),
            });
        } else if score >= UNCERTAIN_SCORE && uncertain.len() < max_uncertain {
            uncertain.push(index);
        }
    }
    (picks, uncertain)
}

/// The `limit` best-scored candidates, in history order, for a listwise screening to choose
/// from.
pub fn top_candidates(candidates: &[usize], scores: &[f32], limit: usize) -> Vec<usize> {
    let mut ranked: Vec<(usize, f32)> = candidates
        .iter()
        .copied()
        .zip(scores.iter().copied())
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut top: Vec<usize> = ranked
        .into_iter()
        .take(limit)
        .map(|(index, _)| index)
        .collect();
    top.sort_unstable();
    top
}

#[cfg(test)]
#[path = "score_tests.rs"]
mod tests;
