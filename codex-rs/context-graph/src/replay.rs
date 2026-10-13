//! Builds a context graph from rollout items, either a whole rollout at once or item by item as a
//! live session persists them. Both paths go through [`GraphRecorder`], so they yield the same
//! graph.

use codex_history::RolloutItem;
use codex_protocol::protocol::EventMsg;
use serde::Serialize;

use crate::AppendOutcome;
use crate::Block;
use crate::BlockId;
use crate::BranchId;
use crate::ContextGraph;
use crate::GraphError;
use crate::MessageClassifier;
use crate::Segmenter;

/// Counts of what a [`GraphRecorder`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReplayStats {
    pub rollout_items: u64,
    pub blocks_added: u64,
    pub blocks_inherited: u64,
    pub blocks_reincluded: u64,
    pub rollbacks: u64,
}

/// A change a [`GraphRecorder`] made, for mirroring into a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphChange {
    BlockAdded(BlockId),
    /// The branch's head moved, or the branch is new.
    BranchUpdated(BranchId),
}

/// Applies one thread's rollout items to its branch of a context graph.
///
/// Compaction keeps the original blocks: its replacement history only adds the summary, and
/// re-included items are recognized by their stable IDs. A rollback moves the branch head back
/// and keeps the removed blocks on a rolled-back branch.
#[derive(Debug)]
pub struct GraphRecorder<C> {
    branch: BranchId,
    segmenter: Segmenter<C>,
    next_ordinal: u64,
    stats: ReplayStats,
}

impl<C: MessageClassifier> GraphRecorder<C> {
    /// Records onto `branch`, which must already exist in the graph passed to each call.
    pub fn new(branch: BranchId, classifier: C) -> Self {
        Self {
            branch,
            segmenter: Segmenter::new(classifier),
            next_ordinal: 0,
            stats: ReplayStats::default(),
        }
    }

    pub fn branch(&self) -> &BranchId {
        &self.branch
    }

    pub fn stats(&self) -> ReplayStats {
        self.stats
    }

    /// Applies one persisted rollout item.
    pub fn observe(
        &mut self,
        graph: &mut ContextGraph,
        item: &RolloutItem,
    ) -> Result<Vec<GraphChange>, GraphError> {
        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        self.stats.rollout_items += 1;

        let mut changes = Vec::new();
        match item {
            RolloutItem::ResponseItem(envelope) => {
                let blocks = self.segmenter.push(ordinal, &envelope.item);
                self.record(graph, blocks, &mut changes)?;
            }
            RolloutItem::Compacted(compacted) => {
                for envelope in compacted.replacement_history.iter().flatten() {
                    let blocks = self.segmenter.push(ordinal, &envelope.item);
                    self.record(graph, blocks, &mut changes)?;
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let blocks = self.segmenter.flush();
                self.record(graph, blocks, &mut changes)?;
                if let Some(abandoned) = graph.rollback(&self.branch, rollback.num_turns)? {
                    self.stats.rollbacks += 1;
                    changes.push(GraphChange::BranchUpdated(abandoned));
                    changes.push(GraphChange::BranchUpdated(self.branch.clone()));
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)) => {
                let blocks = self.segmenter.flush();
                self.record(graph, blocks, &mut changes)?;
            }
            _ => {}
        }
        Ok(changes)
    }

    /// Seals everything still open, for example after the last item of a replayed rollout.
    pub fn finish(&mut self, graph: &mut ContextGraph) -> Result<Vec<GraphChange>, GraphError> {
        let mut changes = Vec::new();
        let blocks = self.segmenter.flush();
        self.record(graph, blocks, &mut changes)?;
        Ok(changes)
    }

    fn record(
        &mut self,
        graph: &mut ContextGraph,
        blocks: Vec<Block>,
        changes: &mut Vec<GraphChange>,
    ) -> Result<(), GraphError> {
        let mut moved = false;
        for block in blocks {
            let id = block.id.clone();
            match graph.append(&self.branch, block)? {
                AppendOutcome::Added => {
                    self.stats.blocks_added += 1;
                    changes.push(GraphChange::BlockAdded(id));
                    moved = true;
                }
                AppendOutcome::Inherited => {
                    self.stats.blocks_inherited += 1;
                    moved = true;
                }
                AppendOutcome::Reincluded => self.stats.blocks_reincluded += 1,
            }
        }
        if moved {
            changes.push(GraphChange::BranchUpdated(self.branch.clone()));
        }
        Ok(())
    }
}

/// Replays one thread's whole rollout onto `branch`, which must already exist in `graph`.
pub fn replay_rollout<'a, C: MessageClassifier>(
    graph: &mut ContextGraph,
    branch: &BranchId,
    items: impl IntoIterator<Item = &'a RolloutItem>,
    classifier: C,
) -> Result<ReplayStats, GraphError> {
    let mut recorder = GraphRecorder::new(branch.clone(), classifier);
    for item in items {
        recorder.observe(graph, item)?;
    }
    recorder.finish(graph)?;
    Ok(recorder.stats())
}

#[cfg(test)]
#[path = "replay_tests.rs"]
mod tests;
