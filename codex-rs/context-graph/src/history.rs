//! The history a recall chooses from: the visible blocks of a branch, with their cards and items,
//! grouped into turns.

use std::collections::HashMap;

use codex_protocol::models::ResponseItem;

use crate::BlockId;
use crate::BlockKind;
use crate::card::BlockCard;
use crate::text::estimate_tokens;
use crate::text::item_text;

/// Tokens counted for each item on top of its text, for roles, call IDs and arguments framing.
const ITEM_OVERHEAD_TOKENS: u32 = 8;

/// One visible block with its card and items.
#[derive(Debug, Clone, Copy)]
pub struct HistoryBlock<'a> {
    pub card: &'a BlockCard,
    pub items: &'a [ResponseItem],
    /// False when a tool call lacks its output or an output its call; such a block can only be
    /// shown as text.
    pub complete: bool,
}

/// The visible blocks of a branch, oldest first, grouped into turns. A turn starts with each
/// user message, since older rollouts do not record turn IDs on their items.
#[derive(Debug, Clone)]
pub struct History<'a> {
    blocks: Vec<HistoryBlock<'a>>,
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
            if block.card.kind == BlockKind::UserMessage {
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
            turns,
            tokens,
            positions,
        }
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
            .position(|block| block.card.kind == BlockKind::UserMessage)
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
