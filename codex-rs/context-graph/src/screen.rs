//! Screening: a model reads the cards of the whole history and picks the blocks a request needs.
//!
//! The catalog lists every recallable block in history order and the request comes last, so
//! consecutive screenings of a growing history share a cached prompt prefix. Only a history too
//! long for the catalog budget is cut down, by lexical search, to the blocks that best match the
//! request; that loses the shared prefix, which long histories cannot keep anyway.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::BlockKind;
use crate::card::BlockCard;
use crate::card::ClaimKind;
use crate::extract::Outcome;
use crate::history::History;
use crate::prompt::AnswerError;
use crate::prompt::ModelPrompt;
use crate::prompt::label_position;
use crate::prompt::parse_answer;
use crate::search::rank_cards;
use crate::text::estimate_tokens;
use crate::text::truncate;
use crate::view::Pick;
use crate::view::Presentation;

const INSTRUCTIONS: &str = include_str!("../prompts/screen_instructions.md");

const MAX_CARD_FILES: usize = 4;
const MAX_CARD_CLAIMS: usize = 3;
const MAX_CLAIM_CHARS: usize = 160;
const MAX_REASON_CHARS: usize = 120;

/// Size limits of a screening prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenLimits {
    /// Tokens the catalog may take before the history is cut down by lexical search.
    pub max_catalog_tokens: u32,
    pub max_summary_chars: usize,
    pub max_uncertain: usize,
}

impl Default for ScreenLimits {
    fn default() -> Self {
        Self {
            max_catalog_tokens: 30_000,
            max_summary_chars: 300,
            max_uncertain: 10,
        }
    }
}

/// A rendered screening request.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenPrompt {
    pub prompt: ModelPrompt,
    /// History position of the block each label `C1`, `C2`, … names.
    pub candidates: Vec<usize>,
    /// Whether the history was cut down to fit the catalog budget.
    pub prefiltered: bool,
    pub catalog_tokens: u32,
    pub max_uncertain: usize,
}

/// Renders the screening prompt for `question` over `history`, whose blocks from `recent_start`
/// on are already shown.
pub fn build_screen_prompt(
    history: &History<'_>,
    question: &str,
    recent_start: usize,
    limits: ScreenLimits,
) -> ScreenPrompt {
    build_screen_prompt_within(history, question, recent_start, limits, None)
}

/// Like [`build_screen_prompt`], with the catalog limited to the history positions in `within`
/// when it is set, such as the blocks of the episodes a first screening chose.
pub fn build_screen_prompt_within(
    history: &History<'_>,
    question: &str,
    recent_start: usize,
    limits: ScreenLimits,
    within: Option<&HashSet<usize>>,
) -> ScreenPrompt {
    let recallable: Vec<usize> = (0..history.len())
        .filter(|index| history.recallable(*index))
        .filter(|index| within.is_none_or(|within| within.contains(index)))
        .collect();
    let lines: Vec<String> = recallable
        .iter()
        .map(|index| card_lines(history.card(*index), limits))
        .collect();
    let line_tokens: Vec<u32> = lines.iter().map(|line| estimate_tokens(line)).collect();
    let total: u32 = line_tokens.iter().sum();

    let (candidates, prefiltered) = if total <= limits.max_catalog_tokens {
        ((0..recallable.len()).collect::<Vec<_>>(), false)
    } else {
        (
            prefilter(history, question, &recallable, &line_tokens, limits),
            true,
        )
    };

    let mut input = String::from("## History\n");
    let mut group = None;
    let mut catalog_tokens = 0;
    for (label, candidate) in candidates.iter().enumerate() {
        let index = recallable[*candidate];
        let key = (history.origin(index), history.turn(index));
        if group != Some(key) {
            group = Some(key);
            match key {
                (Some(branch), _) => {
                    input.push_str(&format!("### From the referenced branch \"{branch}\"\n"));
                }
                (None, turn) => input.push_str(&format!("### Turn {turn}\n")),
            }
        }
        input.push_str(&format!("C{} {}\n", label + 1, lines[*candidate]));
        catalog_tokens += line_tokens[*candidate];
    }
    if candidates.is_empty() {
        input.push_str("(empty)\n");
    }
    input.push_str("\n## Context the agent already has\n");
    if recent_start < history.len() {
        let first = history.turn(recent_start);
        let last = history.last_turn();
        if first == last {
            input.push_str(&format!("Turn {first} is shown in full. "));
        } else {
            input.push_str(&format!("Turns {first} to {last} are shown in full. "));
        }
    }
    input.push_str("The user's constraints and decisions from every turn are listed.\n");
    input.push_str("\n## Request\n");
    input.push_str(question.trim());
    input.push('\n');

    ScreenPrompt {
        prompt: ModelPrompt {
            instructions: INSTRUCTIONS.to_string(),
            input,
            schema_name: "context_selection",
            schema: answer_schema(),
        },
        candidates: candidates
            .into_iter()
            .map(|candidate| recallable[candidate])
            .collect(),
        prefiltered,
        catalog_tokens,
        max_uncertain: limits.max_uncertain,
    }
}

/// Positions in `recallable` that fit the catalog budget: every user message, which outlines the
/// session, then the blocks that best match `question`. Returned in history order.
fn prefilter(
    history: &History<'_>,
    question: &str,
    recallable: &[usize],
    line_tokens: &[u32],
    limits: ScreenLimits,
) -> Vec<usize> {
    let mut kept: HashSet<usize> = HashSet::new();
    let mut tokens = 0;
    for (position, index) in recallable.iter().enumerate() {
        if history.card(*index).kind == BlockKind::UserMessage {
            kept.insert(position);
            tokens += line_tokens[position];
        }
    }
    let cards: Vec<&BlockCard> = recallable
        .iter()
        .map(|index| history.card(*index))
        .collect();
    for (position, _) in rank_cards(&cards, question) {
        if tokens + line_tokens[position] > limits.max_catalog_tokens {
            continue;
        }
        if kept.insert(position) {
            tokens += line_tokens[position];
        }
    }
    let mut kept: Vec<usize> = kept.into_iter().collect();
    kept.sort_unstable();
    kept
}

/// The catalog entry of a card, without its label.
pub(crate) fn card_lines(card: &BlockCard, limits: ScreenLimits) -> String {
    let mut line = format!("[{}] {}", kind_name(card.kind), card.title.trim());
    if card.outcome == Some(Outcome::Failure) {
        line.push_str(" (failed)");
    }
    let summary = card
        .summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !summary.is_empty() && summary != card.title.trim() {
        line.push_str(" — ");
        line.push_str(&truncate(&summary, limits.max_summary_chars));
    }
    let entities = &card.entities;
    let mut seen = HashSet::new();
    let mut files: Vec<&str> = entities
        .files_written
        .iter()
        .chain(&entities.files_read)
        .map(String::as_str)
        .filter(|file| seen.insert(*file))
        .collect();
    if !files.is_empty() {
        let more = files.len().saturating_sub(MAX_CARD_FILES);
        files.truncate(MAX_CARD_FILES);
        line.push_str(&format!(" | files: {}", files.join(", ")));
        if more > 0 {
            line.push_str(&format!(" +{more}"));
        }
    }
    for claim in card.claims.iter().take(MAX_CARD_CLAIMS) {
        line.push_str(&format!(
            "\n    - {}: {}",
            claim_name(claim.kind),
            truncate(&claim.text, MAX_CLAIM_CHARS)
        ));
    }
    line
}

fn kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::UserMessage => "user",
        BlockKind::AssistantMessage => "assistant",
        BlockKind::ToolExchange => "tool",
        BlockKind::InterAgentMessage => "agent",
        BlockKind::Reasoning => "reasoning",
        BlockKind::ContextFragment => "context",
        BlockKind::Compaction => "compaction",
        BlockKind::Merge => "merge",
    }
}

fn claim_name(kind: ClaimKind) -> &'static str {
    match kind {
        ClaimKind::UserConstraint => "constraint",
        ClaimKind::Decision => "decision",
        ClaimKind::Finding => "finding",
        ClaimKind::Todo => "todo",
        ClaimKind::OpenQuestion => "question",
    }
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["selected", "uncertain", "confidence"],
        "properties": {
            "confidence": { "type": "string", "enum": ["high", "medium", "low"] },
            "selected": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["card", "detail", "reason"],
                    "properties": {
                        "card": { "type": "string" },
                        "detail": { "type": "string", "enum": ["full", "summary"] },
                        "reason": { "type": "string" }
                    }
                }
            },
            "uncertain": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// How sure a screening was of its choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    #[default]
    Medium,
    /// The request refers to something the cards do not show, chains several earlier steps,
    /// or the cards are too vague to judge.
    Low,
}

/// The blocks a screening answer picked, most important first, and the blocks it could not
/// decide from their cards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenAnswer {
    pub picks: Vec<Pick>,
    /// History positions of blocks to read before deciding.
    pub uncertain: Vec<usize>,
    pub confidence: Confidence,
}

impl ScreenAnswer {
    /// Whether a stronger model should screen again: the screening said it was unsure, or it
    /// left as many blocks undecided as it could.
    pub fn unsure(&self, max_uncertain: usize) -> bool {
        self.confidence == Confidence::Low || self.uncertain.len() >= max_uncertain.max(1)
    }
}

#[derive(Deserialize)]
struct Answer {
    #[serde(default)]
    selected: Vec<Selected>,
    #[serde(default)]
    uncertain: Vec<String>,
    #[serde(default)]
    confidence: Option<String>,
}

#[derive(Deserialize)]
struct Selected {
    card: String,
    #[serde(default)]
    detail: String,
    #[serde(default)]
    reason: String,
}

/// Parses the model's answer to `prompt`, dropping unknown labels and repeated blocks.
pub fn parse_screen_answer(prompt: &ScreenPrompt, text: &str) -> Result<ScreenAnswer, AnswerError> {
    let answer: Answer = parse_answer(text)?;
    let resolve = |label: &str| {
        label_position(label, 'C', prompt.candidates.len())
            .map(|position| prompt.candidates[position])
    };
    let mut seen = HashSet::new();
    let mut picks = Vec::new();
    for selected in &answer.selected {
        let Some(index) = resolve(&selected.card) else {
            continue;
        };
        if !seen.insert(index) {
            continue;
        }
        let presentation = if selected.detail.trim() == "summary" {
            Presentation::Summary
        } else {
            Presentation::Full
        };
        let reason = selected.reason.trim();
        picks.push(Pick {
            index,
            presentation,
            note: (!reason.is_empty()).then(|| truncate(reason, MAX_REASON_CHARS)),
        });
    }
    let uncertain = answer
        .uncertain
        .iter()
        .filter_map(|label| resolve(label))
        .filter(|index| seen.insert(*index))
        .take(prompt.max_uncertain)
        .collect();
    let confidence = match answer.confidence.as_deref().map(str::trim) {
        Some("high") => Confidence::High,
        Some("low") => Confidence::Low,
        _ => Confidence::Medium,
    };
    Ok(ScreenAnswer {
        picks,
        uncertain,
        confidence,
    })
}

const EPISODE_INSTRUCTIONS: &str = include_str!("../prompts/episode_instructions.md");

/// A first screening over the episodes of a history too long for one catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodePrompt {
    pub prompt: ModelPrompt,
    /// The episode each label `E1`, `E2`, … names, by position in the history's episodes.
    pub episodes: Vec<usize>,
}

/// Renders the episode screening for `question`, or `None` when the history has no episodes
/// before `recent_start`.
pub fn build_episode_prompt(
    history: &History<'_>,
    question: &str,
    recent_start: usize,
) -> Option<EpisodePrompt> {
    let first_recent_turn = (recent_start < history.len()).then(|| history.turn(recent_start));
    let episodes: Vec<usize> = history
        .episodes()
        .iter()
        .enumerate()
        .filter(|(_, span)| first_recent_turn.is_none_or(|turn| span.first_turn < turn))
        .map(|(position, _)| position)
        .collect();
    if episodes.is_empty() {
        return None;
    }
    let mut input = String::from("## Episodes\n");
    for (label, position) in episodes.iter().enumerate() {
        let span = &history.episodes()[*position];
        let turns = if span.first_turn == span.last_turn {
            format!("turn {}", span.first_turn)
        } else {
            format!("turns {}-{}", span.first_turn, span.last_turn)
        };
        let summary = span
            .summary
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        input.push_str(&format!(
            "E{} ({turns}) {} — {}\n",
            label + 1,
            span.title.trim(),
            truncate(&summary, 400)
        ));
    }
    input.push_str("\n## Request\n");
    input.push_str(question.trim());
    input.push('\n');
    Some(EpisodePrompt {
        prompt: ModelPrompt {
            instructions: EPISODE_INSTRUCTIONS.to_string(),
            input,
            schema_name: "episode_selection",
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["episodes"],
                "properties": {
                    "episodes": { "type": "array", "items": { "type": "string" } }
                }
            }),
        },
        episodes,
    })
}

#[derive(Deserialize)]
struct EpisodeAnswer {
    #[serde(default)]
    episodes: Vec<String>,
}

/// The history positions a second screening chooses from after the episode screening answered
/// `text`: the blocks of the chosen episodes, of turns no episode covers, and every user
/// message, which outlines the session.
pub fn parse_episode_answer(
    history: &History<'_>,
    prompt: &EpisodePrompt,
    text: &str,
) -> Result<HashSet<usize>, AnswerError> {
    let answer: EpisodeAnswer = parse_answer(text)?;
    let chosen: Vec<&crate::history::EpisodeSpan> = answer
        .episodes
        .iter()
        .filter_map(|label| label_position(label, 'E', prompt.episodes.len()))
        .map(|position| &history.episodes()[prompt.episodes[position]])
        .collect();
    let covered = |turn: usize| {
        history
            .episodes()
            .iter()
            .any(|span| span.first_turn <= turn && turn <= span.last_turn)
    };
    Ok((0..history.len())
        .filter(|index| {
            let turn = history.turn(*index);
            history.card(*index).kind == BlockKind::UserMessage
                || !covered(turn)
                || chosen
                    .iter()
                    .any(|span| span.first_turn <= turn && turn <= span.last_turn)
        })
        .collect())
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod tests;
