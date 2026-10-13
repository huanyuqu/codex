//! A branch's history at one moment: its blocks in order, with their items and cards.

use std::collections::HashMap;

use codex_context_graph::Block;
use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use codex_context_graph::ObservedBlock;
use codex_context_graph::card::BlockCard;
use codex_context_graph::extract::extract_facts;
use codex_context_graph::history::History;
use codex_context_graph::history::HistoryBlock;
use codex_protocol::models::ResponseItem;

/// The blocks of a branch in history order, with their items and cards.
#[derive(Debug, Clone)]
pub struct BranchSnapshot {
    pub graph_id: String,
    pub branch: BranchId,
    pub blocks: Vec<Block>,
    pub items: Vec<Vec<ResponseItem>>,
    pub cards: Vec<BlockCard>,
    /// Blocks that had no stored card and got an extracted one.
    pub extracted_cards: usize,
}

impl BranchSnapshot {
    /// Pairs `observed` blocks, in history order, with their cards from `stored`, extracting
    /// cards for blocks that have none yet, such as blocks the indexer has not reached.
    pub fn new(
        graph_id: impl Into<String>,
        branch: BranchId,
        observed: Vec<ObservedBlock>,
        mut stored: HashMap<BlockId, BlockCard>,
    ) -> Self {
        let mut snapshot = Self {
            graph_id: graph_id.into(),
            branch,
            blocks: Vec::with_capacity(observed.len()),
            items: Vec::with_capacity(observed.len()),
            cards: Vec::with_capacity(observed.len()),
            extracted_cards: 0,
        };
        for ObservedBlock {
            block, items, cwd, ..
        } in observed
        {
            let card = stored.remove(&block.id).unwrap_or_else(|| {
                snapshot.extracted_cards += 1;
                let facts = extract_facts(block.kind, &items, cwd.as_deref());
                BlockCard::extracted(&block, facts, &items)
            });
            snapshot.blocks.push(block);
            snapshot.items.push(items);
            snapshot.cards.push(card);
        }
        snapshot
    }

    /// The history up to the end of turn `until_turn`, or all of it.
    pub fn history(&self, until_turn: Option<usize>) -> History<'_> {
        let blocks: Vec<HistoryBlock<'_>> = self
            .cards
            .iter()
            .zip(&self.items)
            .zip(&self.blocks)
            .map(|((card, items), block)| HistoryBlock {
                card,
                items,
                complete: block.complete,
            })
            .collect();
        let full = History::new(blocks.clone());
        let Some(until_turn) = until_turn else {
            return full;
        };
        let end = (0..full.len())
            .find(|index| full.turn(*index) > until_turn)
            .unwrap_or(full.len());
        History::new(blocks[..end].to_vec())
    }
}
