//! The history a recall chooses from: the visible blocks of a branch, with their cards and items,
//! grouped into turns.

use std::collections::HashMap;

use codex_protocol::models::ResponseItem;

use crate::BlockId;
use crate::BlockKind;
use crate::card::BlockCard;
use crate::episode::EpisodeCard;
use crate::text::estimate_tokens;
use crate::text::item_text;

/// Tokens counted for each item on top of its text, for roles, call IDs and arguments framing.
const ITEM_OVERHEAD_TOKENS: u32 = 8;

/// Turns of a history that an episode card sums up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeSpan {
    pub first_turn: usize,
    pub last_turn: usize,
    pub title: String,
    pub summary: String,
}

/// One visible block with its card and items.
#[derive(Debug, Clone, Copy)]
pub struct HistoryBlock<'a> {
    pub card: &'a BlockCard,
    pub items: &'a [ResponseItem],
    /// False when a tool call lacks its output or an output its call; such a block can only be
    /// shown as text.
    pub complete: bool,
    /// The branch a referenced block comes from; `None` for the history's own blocks.
    pub origin: Option<&'a str>,
    /// Whether a file the block read has changed since, which makes the block outdated.
    pub changed: bool,
}

/// The visible blocks of a branch, oldest first, grouped into turns. A turn starts with each
/// user message, since older rollouts do not record turn IDs on their items.
///
/// Blocks of branches the branch references sit where the reference was made. They keep the
/// turn of their place and never start one, and they are only shown when recall picks them.
#[derive(Debug, Clone)]
pub struct History<'a> {
    blocks: Vec<HistoryBlock<'a>>,
    /// Runs of turns that episode cards sum up, in order and without overlaps.
    episodes: Vec<EpisodeSpan>,
    /// Turn of each block. Blocks before the first user message, such as the initial harness
    /// context, are in turn 0.
    turns: Vec<usize>,
    tokens: Vec<u32>,
    positions: HashMap<BlockId, usize>,
}

impl<'a> History<'a> {
    pub fn new(blocks: Vec<HistoryBlock<'a>>) -> Self {
        let mut turns = Vec::with_capacity(blocks.len());
        let mut turn = 0;
        for block in &blocks {
            if block.card.kind == BlockKind::UserMessage && block.origin.is_none() {
                turn += 1;
            }
            turns.push(turn);
        }
        let tokens = blocks
            .iter()
            .map(|block| block_tokens(block.items))
            .collect();
        let positions = blocks
            .iter()
            .enumerate()
            .map(|(index, block)| (block.card.block_id.clone(), index))
            .collect();
        Self {
            blocks,
            episodes: Vec::new(),
            turns,
            tokens,
            positions,
        }
    }

    /// Adds the episodes of `cards` whose turns this history holds.
    pub fn with_episodes(mut self, cards: &[EpisodeCard]) -> Self {
        let turn_of = |id: &BlockId| {
            self.position(id)
                .filter(|index| self.is_own(*index))
                .map(|index| self.turn(index))
        };
        let mut spans: Vec<EpisodeSpan> = cards
            .iter()
            .filter_map(|card| {
                let first_turn = turn_of(&card.first_block)?;
                let last_turn = turn_of(&card.last_block)?;
                (first_turn <= last_turn).then(|| EpisodeSpan {
                    first_turn,
                    last_turn,
                    title: card.title.clone(),
                    summary: card.summary.clone(),
                })
            })
            .collect();
        spans.sort_by_key(|span| (span.first_turn, span.last_turn));
        let mut end = 0;
        spans.retain(|span| {
            let keep = span.first_turn > end;
            if keep {
                end = span.last_turn;
            }
            keep
        });
        self.episodes = spans;
        self
    }

    pub fn episodes(&self) -> &[EpisodeSpan] {
        &self.episodes
    }

    pub fn blocks(&self) -> &[HistoryBlock<'a>] {
        &self.blocks
    }

    pub fn block(&self, index: usize) -> &HistoryBlock<'a> {
        &self.blocks[index]
    }

    pub fn card(&self, index: usize) -> &'a BlockCard {
        self.blocks[index].card
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn turn(&self, index: usize) -> usize {
        self.turns[index]
    }

    pub fn last_turn(&self) -> usize {
        self.turns.last().copied().unwrap_or_default()
    }

    /// Estimated tokens of the block's items as the model sees them.
    pub fn tokens(&self, index: usize) -> u32 {
        self.tokens[index]
    }

    pub fn position(&self, id: &BlockId) -> Option<usize> {
        self.positions.get(id).copied()
    }

    /// Index of the first block of the last `turns` turns, or the length when `turns` is 0.
    pub fn recent_start(&self, turns: usize) -> usize {
        if turns == 0 {
            return self.len();
        }
        let first_recent = (self.last_turn() + 1).saturating_sub(turns);
        self.turns
            .iter()
            .position(|turn| *turn >= first_recent)
            .unwrap_or(self.len())
    }

    /// Index of the first user message, the task the branch started with.
    pub fn task_start(&self) -> Option<usize> {
        self.blocks
            .iter()
            .position(|block| block.card.kind == BlockKind::UserMessage && block.origin.is_none())
    }

    /// The branch the block at `index` comes from, when it is a referenced block.
    pub fn origin(&self, index: usize) -> Option<&'a str> {
        self.blocks[index].origin
    }

    /// Whether a file the block at `index` read has changed since.
    pub fn changed(&self, index: usize) -> bool {
        self.blocks[index].changed
    }

    /// Whether the block at `index` is one of the history's own, not a referenced block.
    pub fn is_own(&self, index: usize) -> bool {
        self.blocks[index].origin.is_none()
    }

    /// Whether the block at `index` is in the recent turns that start at `recent_start`, which
    /// views show in full. Referenced blocks never are.
    pub fn is_recent(&self, index: usize, recent_start: usize) -> bool {
        index >= recent_start && self.is_own(index)
    }

    /// Whether recall may show this block. Harness context is regenerated for every request,
    /// and compaction summaries restate blocks that the history still holds.
    pub fn recallable(&self, index: usize) -> bool {
        !matches!(
            self.blocks[index].card.kind,
            BlockKind::ContextFragment | BlockKind::Compaction | BlockKind::Reasoning
        )
    }
}

fn block_tokens(items: &[ResponseItem]) -> u32 {
    items
        .iter()
        .filter(|item| !matches!(item, ResponseItem::Reasoning { .. }))
        .map(|item| {
            item_text(item).map_or(0, |text| estimate_tokens(&text.text)) + ITEM_OVERHEAD_TOKENS
        })
        .sum()
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
