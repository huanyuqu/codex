//! The prompt an indexing model gets for a batch of blocks, and validation of its answer.
//!
//! The instructions never change and come first, so batches share a cached prompt prefix. The
//! model sees each block's extracted card facts and a head-and-tail rendering of its items, plus
//! a catalog of earlier blocks it may link to. Blocks are named by short labels, `B1` for the
//! batch and `E1` for the catalog, because models copy long block IDs unreliably.

use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::BlockId;
use crate::card::BlockCard;
use crate::card::Claim;
use crate::card::ClaimKind;
use crate::card::Relation;
use crate::prompt::ModelPrompt;
use crate::prompt::strip_code_fence;
use crate::text::render_block;
use crate::text::truncate;

const INSTRUCTIONS: &str = include_str!("../prompts/card_instructions.md");

const MAX_TITLE_CHARS: usize = 100;
const MAX_SUMMARY_CHARS: usize = 600;
const MAX_CLAIMS: usize = 6;
const MAX_CLAIM_CHARS: usize = 300;
const MAX_LINKS: usize = 3;

/// Size limits for one batch prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptLimits {
    pub max_item_chars: usize,
    pub max_block_chars: usize,
    pub max_catalog_lines: usize,
}

impl Default for PromptLimits {
    fn default() -> Self {
        Self {
            max_item_chars: 3_000,
            max_block_chars: 6_000,
            max_catalog_lines: 40,
        }
    }
}

/// A rendered indexing request.
#[derive(Debug, Clone, PartialEq)]
pub struct CardPrompt {
    pub prompt: ModelPrompt,
    /// Blocks the answer must describe, in order.
    pub batch: Vec<BlockId>,
    /// Blocks the answer may link to: the catalog and the batch.
    pub linkable: Vec<BlockId>,
    /// The label each block of `linkable` has in the prompt.
    pub labels: Vec<String>,
}

/// Renders the prompt for `batch`. `earlier` are cards of blocks before the batch, oldest first;
/// the newest of them form the catalog the model may link to.
pub fn build_card_prompt(
    batch: &[(&BlockCard, &[ResponseItem])],
    earlier: &[&BlockCard],
    limits: PromptLimits,
) -> CardPrompt {
    let catalog_start = earlier.len().saturating_sub(limits.max_catalog_lines);
    let catalog = &earlier[catalog_start..];
    let mut labels = Vec::with_capacity(catalog.len() + batch.len());
    let mut input = String::from("## Earlier blocks\n");
    if catalog.is_empty() {
        input.push_str("(none)\n");
    }
    for (index, card) in catalog.iter().enumerate() {
        let label = format!("E{}", index + 1);
        input.push_str(&card.catalog_entry(&label));
        input.push('\n');
        labels.push(label);
    }
    input.push_str("\n## Blocks to index\n");
    for (index, (card, items)) in batch.iter().enumerate() {
        let label = format!("B{}", index + 1);
        input.push_str(&format!(
            "\n### {label} [{}]\n",
            crate::render::kind_name(card.kind)
        ));
        labels.push(label);
        let facts = fact_line(card);
        if !facts.is_empty() {
            input.push_str(&format!("facts: {facts}\n"));
        }
        input.push_str(&render_block(
            items,
            limits.max_item_chars,
            limits.max_block_chars,
        ));
    }

    let batch_ids: Vec<BlockId> = batch
        .iter()
        .map(|(card, _)| card.block_id.clone())
        .collect();
    let linkable = catalog
        .iter()
        .map(|card| card.block_id.clone())
        .chain(batch_ids.iter().cloned())
        .collect();
    CardPrompt {
        prompt: ModelPrompt {
            instructions: INSTRUCTIONS.to_string(),
            input,
            schema_name: "index_cards",
            schema: answer_schema(),
        },
        batch: batch_ids,
        linkable,
        labels,
    }
}

fn fact_line(card: &BlockCard) -> String {
    let mut parts = Vec::new();
    if let Some(code) = card.exit_code {
        parts.push(format!("exit code {code}"));
    }
    let entities = &card.entities;
    if !entities.files_written.is_empty() {
        parts.push(format!("wrote {}", entities.files_written.join(", ")));
    }
    if !entities.files_read.is_empty() {
        parts.push(format!("read {}", entities.files_read.join(", ")));
    }
    parts.join("; ")
}

fn answer_schema() -> Value {
    let ids = json!({ "type": "array", "items": { "type": "string" } });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["cards"],
        "properties": {
            "cards": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["block_id", "title", "summary", "claims", "depends_on", "supersedes"],
                    "properties": {
                        "block_id": { "type": "string" },
                        "title": { "type": "string" },
                        "summary": { "type": "string" },
                        "claims": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["kind", "text"],
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["user_constraint", "decision", "finding", "todo", "open_question"]
                                    },
                                    "text": { "type": "string" }
                                }
                            }
                        },
                        "depends_on": ids,
                        "supersedes": ids
                    }
                }
            }
        }
    })
}

/// A card as the indexing model wrote it, after validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCard {
    pub block_id: BlockId,
    pub title: String,
    pub summary: String,
    pub claims: Vec<Claim>,
    pub depends_on: Vec<BlockId>,
    pub supersedes: Vec<BlockId>,
}

impl ModelCard {
    /// Replaces the title and summary of `card`, adds the claims and links, and records
    /// `indexer` as the model that wrote them. Extracted facts and links are kept.
    pub fn apply_to(self, card: &mut BlockCard, indexer: &str) {
        if !self.title.is_empty() {
            card.title = self.title;
        }
        if !self.summary.is_empty() {
            card.summary = self.summary;
        }
        card.claims = self.claims;
        card.link(Relation::DependsOn, self.depends_on);
        card.link(Relation::Supersedes, self.supersedes);
        card.indexer = indexer.to_string();
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CardParseError {
    #[error("indexing answer is not valid JSON ({error}): {start:?}")]
    Json { error: String, start: String },
    #[error("indexing answer has no card for any block of the batch")]
    Empty,
}

/// The validated cards of an answer, and the batch blocks it did not describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardAnswer {
    pub cards: Vec<ModelCard>,
    pub missing: Vec<BlockId>,
}

#[derive(Deserialize)]
struct Answer {
    cards: Vec<AnswerCard>,
}

#[derive(Deserialize)]
struct AnswerCard {
    block_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    claims: Vec<AnswerClaim>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    supersedes: Vec<String>,
}

#[derive(Deserialize)]
struct AnswerClaim {
    kind: String,
    text: String,
}

/// Parses the model's answer to `prompt`. Cards for blocks outside the batch, unknown claim
/// kinds, and links to blocks that are not earlier and linkable are dropped, and blocks without
/// a card are reported as missing. Fails when the answer is not JSON or describes no block of the
/// batch.
pub fn parse_card_answer(prompt: &CardPrompt, text: &str) -> Result<CardAnswer, CardParseError> {
    let json = strip_code_fence(text);
    let answer: Answer = serde_json::from_str(json).map_err(|err| CardParseError::Json {
        error: err.to_string(),
        start: truncate(json, 80),
    })?;
    // Answers name blocks by label; block IDs are accepted too.
    let position = |name: &str| {
        let name = name.trim();
        prompt
            .labels
            .iter()
            .position(|label| label == name)
            .or_else(|| prompt.linkable.iter().position(|id| id.as_str() == name))
    };
    let catalog_len = prompt.linkable.len() - prompt.batch.len();

    let mut cards = Vec::new();
    let mut missing = Vec::new();
    for (index, block_id) in prompt.batch.iter().enumerate() {
        let own = catalog_len + index;
        let Some(answer) = answer
            .cards
            .iter()
            .find(|card| position(&card.block_id) == Some(own))
        else {
            missing.push(block_id.clone());
            continue;
        };
        // A block may link to the catalog and to batch blocks before it.
        let links = |names: &[String]| -> Vec<BlockId> {
            let mut kept: Vec<BlockId> = Vec::new();
            for linked in names.iter().filter_map(|name| position(name)) {
                let id = &prompt.linkable[linked];
                if linked < own && !kept.contains(id) {
                    kept.push(id.clone());
                }
            }
            kept.truncate(MAX_LINKS);
            kept
        };
        cards.push(ModelCard {
            block_id: block_id.clone(),
            title: truncate(answer.title.trim(), MAX_TITLE_CHARS),
            summary: truncate(answer.summary.trim(), MAX_SUMMARY_CHARS),
            claims: answer
                .claims
                .iter()
                .filter_map(|claim| {
                    let kind = serde_json::from_value::<ClaimKind>(Value::String(
                        claim.kind.trim().to_string(),
                    ))
                    .ok()?;
                    let text = claim.text.trim();
                    (!text.is_empty()).then(|| Claim {
                        kind,
                        text: truncate(text, MAX_CLAIM_CHARS),
                    })
                })
                .take(MAX_CLAIMS)
                .collect(),
            depends_on: links(&answer.depends_on),
            supersedes: links(&answer.supersedes),
        });
    }
    if cards.is_empty() {
        return Err(CardParseError::Empty);
    }
    Ok(CardAnswer { cards, missing })
}

#[cfg(test)]
#[path = "card_prompt_tests.rs"]
mod tests;
