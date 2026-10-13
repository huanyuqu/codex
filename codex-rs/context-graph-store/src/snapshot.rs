//! A branch's history at one moment: its blocks in order, with their items and cards, and the
//! blocks of the branches it references.

use std::collections::HashMap;
use std::collections::HashSet;

use codex_context_graph::AppendOutcome;
use codex_context_graph::Block;
use codex_context_graph::BlockId;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphRecorder;
use codex_context_graph::ObservedBlock;
use codex_context_graph::RecorderEvent;
use codex_context_graph::card::BlockCard;
use codex_context_graph::episode::EpisodeCard;
use codex_context_graph::extract::extract_facts;
use codex_context_graph::history::History;
use codex_context_graph::history::HistoryBlock;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;

use crate::indexer::file_changed;

/// The blocks of a branch in history order, with their items and cards.
#[derive(Debug, Clone)]
pub struct BranchSnapshot {
    pub graph_id: String,
    pub branch: BranchId,
    pub blocks: Vec<Block>,
    pub items: Vec<Vec<ResponseItem>>,
    pub cards: Vec<BlockCard>,
    /// The referenced branch each block comes from; `None` for the branch's own blocks.
    pub origins: Vec<Option<String>>,
    /// Whether a file each block read has changed since the block read it.
    pub changed: Vec<bool>,
    /// Blocks that had no stored card and got an extracted one.
    pub extracted_cards: usize,
    /// Episode cards of the graph; the history uses those of its own turns.
    pub episodes: Vec<EpisodeCard>,
}

impl BranchSnapshot {
    /// Pairs `observed` blocks, in history order, with their cards from `stored`, extracting
    /// cards for blocks that have none yet, such as blocks the indexer has not reached.
    pub fn new(
        graph_id: impl Into<String>,
        branch: BranchId,
        observed: Vec<ObservedBlock>,
        stored: &HashMap<BlockId, BlockCard>,
    ) -> Self {
        let mut snapshot = Self {
            graph_id: graph_id.into(),
            branch,
            blocks: Vec::with_capacity(observed.len()),
            items: Vec::with_capacity(observed.len()),
            cards: Vec::with_capacity(observed.len()),
            origins: Vec::with_capacity(observed.len()),
            changed: Vec::with_capacity(observed.len()),
            extracted_cards: 0,
            episodes: Vec::new(),
        };
        for block in observed {
            let position = snapshot.blocks.len();
            snapshot.insert(position, block, stored, None);
        }
        snapshot
    }

    fn insert(
        &mut self,
        position: usize,
        observed: ObservedBlock,
        stored: &HashMap<BlockId, BlockCard>,
        origin: Option<&str>,
    ) {
        let ObservedBlock {
            block, items, cwd, ..
        } = observed;
        let card = stored.get(&block.id).cloned().unwrap_or_else(|| {
            self.extracted_cards += 1;
            let facts = extract_facts(block.kind, &items, cwd.as_deref());
            BlockCard::extracted(&block, facts, &items)
        });
        self.blocks.insert(position, block);
        self.items.insert(position, items);
        self.cards.insert(position, card);
        self.origins.insert(position, origin.map(str::to_string));
        self.changed.insert(position, false);
    }

    /// Marks the blocks that read a file which changed since; recall treats them as outdated.
    pub fn mark_changed_files(&mut self) {
        for (card, changed) in self.cards.iter().zip(self.changed.iter_mut()) {
            *changed = card.file_stamps.iter().any(file_changed);
        }
    }

    /// Adds the blocks of the referenced branch `origin` that this history does not already
    /// hold, after the block `after` where the reference was made, or at the end.
    pub fn add_reference(
        &mut self,
        origin: &str,
        after: Option<&BlockId>,
        observed: Vec<ObservedBlock>,
        stored: &HashMap<BlockId, BlockCard>,
    ) {
        let held: HashSet<BlockId> = self.blocks.iter().map(|block| block.id.clone()).collect();
        let mut position = after
            .and_then(|after| self.blocks.iter().position(|block| &block.id == after))
            .map_or(self.blocks.len(), |index| index + 1);
        // Earlier references made at the same place stay first.
        while self.origins.get(position).is_some_and(Option::is_some) {
            position += 1;
        }
        for block in observed {
            if held.contains(&block.block.id) {
                continue;
            }
            self.insert(position, block, stored, Some(origin));
            position += 1;
        }
    }

    /// The history up to the end of turn `until_turn`, or all of it.
    pub fn history(&self, until_turn: Option<usize>) -> History<'_> {
        let blocks: Vec<HistoryBlock<'_>> = self
            .cards
            .iter()
            .zip(&self.items)
            .zip(&self.blocks)
            .zip(&self.origins)
            .zip(&self.changed)
            .map(|((((card, items), block), origin), changed)| HistoryBlock {
                card,
                items,
                complete: block.complete,
                origin: origin.as_deref(),
                changed: *changed,
            })
            .collect();
        let full = History::new(blocks.clone()).with_episodes(&self.episodes);
        let Some(until_turn) = until_turn else {
            return full;
        };
        let end = (0..full.len())
            .find(|index| full.turn(*index) > until_turn)
            .unwrap_or(full.len());
        History::new(blocks[..end].to_vec()).with_episodes(&self.episodes)
    }
}

/// Replays a thread's rollout `items` and returns its blocks in history order with their items,
/// up to and including `until`, when the history reaches it.
pub fn replay_blocks(
    thread_id: &str,
    items: &[RolloutItem],
    until: Option<&BlockId>,
) -> anyhow::Result<Vec<ObservedBlock>> {
    let branch = BranchId::for_thread(thread_id);
    let mut graph = ContextGraph::new();
    graph.add_branch(Branch::new(branch.clone(), thread_id))?;
    let mut recorder = GraphRecorder::new(branch.clone(), DefaultClassifier).with_events();
    let mut observed: HashMap<BlockId, ObservedBlock> = HashMap::new();
    let mut keep = |recorder: &mut GraphRecorder<DefaultClassifier>| {
        for event in recorder.take_events() {
            if let RecorderEvent::Block(block) = event
                && block.outcome != AppendOutcome::Reincluded
            {
                observed.insert(block.block.id.clone(), block);
            }
        }
    };
    for item in items {
        recorder.observe(&mut graph, item)?;
        keep(&mut recorder);
    }
    recorder.finish(&mut graph)?;
    keep(&mut recorder);
    let Some(head) = graph.branch(&branch).and_then(|branch| branch.head.clone()) else {
        return Ok(Vec::new());
    };
    let mut path = graph.primary_path(&head)?;
    if let Some(until) = until
        && let Some(end) = path.iter().position(|id| id == until)
    {
        path.truncate(end + 1);
    }
    Ok(path.iter().filter_map(|id| observed.remove(id)).collect())
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
