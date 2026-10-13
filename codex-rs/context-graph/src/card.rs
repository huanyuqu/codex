//! Index cards: short, structured descriptions of blocks that recall screens instead of reading
//! the blocks themselves.
//!
//! Every block first gets an extracted card built from [`BlockFacts`] alone. An indexing model
//! may later replace its title and summary and add claims and links. Cards are derived data:
//! they can always be rebuilt from the blocks.

use codex_protocol::models::ResponseItem;
use serde::Deserialize;
use serde::Serialize;

use crate::Block;
use crate::BlockId;
use crate::BlockKind;
use crate::extract::BlockEntities;
use crate::extract::BlockFacts;
use crate::extract::Outcome;
use crate::text::first_line;
use crate::text::item_text;
use crate::text::truncate;

/// Version of the card schema and prompts. Cards written by an older version are rebuilt.
pub const CARD_VERSION: u32 = 1;

/// The indexer name of cards built without a model.
pub const EXTRACTED_INDEXER: &str = "extract";

const TITLE_CHARS: usize = 100;
const SUMMARY_CHARS: usize = 320;

/// What a claim asserts. Recall always keeps user constraints and decisions in view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// A requirement the user stated, such as "do not change the public API".
    UserConstraint,
    /// A design or implementation choice that was made.
    Decision,
    /// Something learned, such as a root cause or a measurement.
    Finding,
    /// Work that is planned or still open.
    Todo,
    OpenQuestion,
}

/// A statement a block makes that later work may depend on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub kind: ClaimKind,
    pub text: String,
}

/// The index card of one block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCard {
    pub block_id: BlockId,
    pub kind: BlockKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub card_version: u32,
    /// [`EXTRACTED_INDEXER`], or the model that wrote the title, summary and claims.
    pub indexer: String,
    pub title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "BlockEntities::is_empty")]
    pub entities: BlockEntities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claims: Vec<Claim>,
    /// Earlier blocks this block needs to be understood, such as the read before an edit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<BlockId>,
    /// Earlier blocks this block makes stale, such as an older read of a file it changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<BlockId>,
    pub tokens: u32,
}

impl BlockCard {
    /// Builds the extracted card of a block from its facts and items.
    pub fn extracted(block: &Block, facts: BlockFacts, items: &[ResponseItem]) -> Self {
        let (title, summary) = extracted_text(block.kind, &facts, items);
        Self {
            block_id: block.id.clone(),
            kind: block.kind,
            turn_id: block.turn_id.clone(),
            card_version: CARD_VERSION,
            indexer: EXTRACTED_INDEXER.to_string(),
            title,
            summary,
            entities: facts.entities,
            tools: facts.tools,
            outcome: facts.outcome,
            exit_code: facts.exit_code,
            claims: Vec::new(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            tokens: facts.tokens,
        }
    }

    /// Whether an indexing model should describe this block. Harness context is regenerated
    /// rather than recalled, and blocks without readable text have nothing to describe.
    pub fn wants_model(&self) -> bool {
        match self.kind {
            BlockKind::ContextFragment | BlockKind::Reasoning => false,
            BlockKind::Compaction => !self.summary.is_empty(),
            _ => true,
        }
    }

    /// Adds `ids` to `depends_on` or `supersedes`, keeping order and skipping duplicates and
    /// self-references.
    pub fn link(&mut self, relation: Relation, ids: impl IntoIterator<Item = BlockId>) {
        let own = self.block_id.clone();
        let list = match relation {
            Relation::DependsOn => &mut self.depends_on,
            Relation::Supersedes => &mut self.supersedes,
        };
        for id in ids {
            if id != own && !list.contains(&id) {
                list.push(id);
            }
        }
    }

    /// One line for card catalogs: `blk_… [kind] title`, with the outcome when it failed.
    pub fn catalog_line(&self) -> String {
        self.catalog_entry(self.block_id.as_str())
    }

    /// A catalog line that names the block by `label` instead of its ID.
    pub fn catalog_entry(&self, label: &str) -> String {
        let failed = if self.outcome == Some(Outcome::Failure) {
            " (failed)"
        } else {
            ""
        };
        format!(
            "{label} [{}] {}{failed}",
            crate::render::kind_name(self.kind),
            self.title
        )
    }
}

/// A link between two cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    DependsOn,
    Supersedes,
}

fn extracted_text(kind: BlockKind, facts: &BlockFacts, items: &[ResponseItem]) -> (String, String) {
    let texts: Vec<String> = items
        .iter()
        .filter_map(item_text)
        .filter(|text| !text.label.starts_with("reasoning"))
        .map(|text| text.text)
        .collect();
    let joined = texts.join("\n");
    match kind {
        BlockKind::ToolExchange => {
            let entities = &facts.entities;
            let title = entities
                .commands
                .first()
                .map(|command| format!("$ {}", first_line(command, TITLE_CHARS)))
                .or_else(|| {
                    (!entities.files_written.is_empty()).then(|| {
                        format!(
                            "edit {}",
                            truncate(&short_paths(&entities.files_written), TITLE_CHARS)
                        )
                    })
                })
                .or_else(|| facts.tools.first().cloned())
                .unwrap_or_else(|| first_line(&joined, TITLE_CHARS));
            let mut parts = Vec::new();
            if let Some(code) = facts.exit_code {
                parts.push(format!("exit {code}"));
            }
            if !entities.files_written.is_empty() {
                parts.push(format!("wrote {}", short_paths(&entities.files_written)));
            }
            if !entities.files_read.is_empty() {
                parts.push(format!("read {}", short_paths(&entities.files_read)));
            }
            if let Some(error) = entities.errors.first() {
                parts.push(error.clone());
            }
            (title, truncate(&parts.join("; "), SUMMARY_CHARS))
        }
        _ => (
            first_line(&joined, TITLE_CHARS),
            truncate(
                &joined.split_whitespace().collect::<Vec<_>>().join(" "),
                SUMMARY_CHARS,
            ),
        ),
    }
}

/// File names of at most three paths, followed by how many more there are.
fn short_paths(paths: &[String]) -> String {
    let mut names: Vec<&str> = paths
        .iter()
        .take(3)
        .map(|path| path.rsplit('/').next().unwrap_or(path))
        .collect();
    let more = paths.len().saturating_sub(3);
    let more = format!("+{more}");
    if paths.len() > 3 {
        names.push(&more);
    }
    names.join(", ")
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
