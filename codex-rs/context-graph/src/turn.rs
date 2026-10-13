//! Turn cards: one card per turn, summarizing what the user asked and what was done.

use serde::Deserialize;
use serde::Serialize;

use crate::BlockId;
use crate::BlockKind;
use crate::card::BlockCard;
use crate::card::EXTRACTED_INDEXER;
use crate::extract::Outcome;
use crate::text::truncate;

const REQUEST_CHARS: usize = 400;
const RESULT_CHARS: usize = 400;
const MAX_TURN_FILES: usize = 20;

/// The card of one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCard {
    pub turn_id: String,
    pub indexer: String,
    pub blocks: Vec<BlockId>,
    /// What the user asked, from the turn's user messages.
    pub request: String,
    /// What the turn ended with, from its last assistant message.
    pub result: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_written: Vec<String>,
    pub commands: u32,
    pub failed_commands: u32,
    pub tokens: u32,
}

/// Collects the cards of the current turn until it ends.
#[derive(Debug, Default, Clone)]
pub struct TurnAccumulator {
    cards: Vec<BlockCard>,
    turns_seen: u64,
}

impl TurnAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, card: &BlockCard) {
        if card.kind != BlockKind::ContextFragment {
            self.cards.push(card.clone());
        }
    }

    /// Ends the turn and returns its card, or `None` when the turn produced no blocks.
    ///
    /// Turns without a recorded ID are numbered in the order they ended.
    pub fn finish(&mut self, turn_id: Option<&str>) -> Option<TurnCard> {
        self.turns_seen += 1;
        let cards = std::mem::take(&mut self.cards);
        if cards.is_empty() {
            return None;
        }
        let turn_id = turn_id
            .map(str::to_string)
            .or_else(|| cards.iter().find_map(|card| card.turn_id.clone()))
            .unwrap_or_else(|| format!("turn-{}", self.turns_seen));

        let request = cards
            .iter()
            .filter(|card| card.kind == BlockKind::UserMessage)
            .map(|card| card.summary.as_str())
            .collect::<Vec<_>>()
            .join(" / ");
        let result = cards
            .iter()
            .rev()
            .find(|card| card.kind == BlockKind::AssistantMessage)
            .map(|card| card.summary.clone())
            .unwrap_or_default();
        let mut files_read = Vec::new();
        let mut files_written = Vec::new();
        let mut commands = 0;
        let mut failed_commands = 0;
        let mut tokens = 0u32;
        for card in &cards {
            for file in &card.entities.files_read {
                if files_read.len() < MAX_TURN_FILES && !files_read.contains(file) {
                    files_read.push(file.clone());
                }
            }
            for file in &card.entities.files_written {
                if files_written.len() < MAX_TURN_FILES && !files_written.contains(file) {
                    files_written.push(file.clone());
                }
            }
            if card.exit_code.is_some() {
                commands += 1;
                if card.outcome == Some(Outcome::Failure) {
                    failed_commands += 1;
                }
            }
            tokens = tokens.saturating_add(card.tokens);
        }
        Some(TurnCard {
            turn_id,
            indexer: EXTRACTED_INDEXER.to_string(),
            blocks: cards.iter().map(|card| card.block_id.clone()).collect(),
            request: truncate(&request, REQUEST_CHARS),
            result: truncate(&result, RESULT_CHARS),
            files_read,
            files_written,
            commands,
            failed_commands,
            tokens,
        })
    }
}
