//! Builds a context graph from rollout items, either a whole rollout at once or item by item as a
//! live session persists them. Both paths go through [`GraphRecorder`], so they yield the same
//! graph.

use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;
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
use crate::segmenter::SealedBlock;

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

/// A block a [`GraphRecorder`] sealed, with the items it was cut from.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedBlock {
    pub block: Block,
    pub outcome: AppendOutcome,
    pub items: Vec<ResponseItem>,
    /// Working directory of the turn that produced the block, for resolving relative paths.
    pub cwd: Option<String>,
}

/// What a [`GraphRecorder`] saw, for consumers that index block content.
#[derive(Debug, Clone, PartialEq)]
pub enum RecorderEvent {
    Block(ObservedBlock),
    /// A turn ended, and every block it produced has been sealed.
    TurnEnded {
        turn_id: Option<String>,
    },
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
    events: Option<Vec<RecorderEvent>>,
    cwd: Option<String>,
}

impl<C: MessageClassifier> GraphRecorder<C> {
    /// Records onto `branch`, which must already exist in the graph passed to each call.
    pub fn new(branch: BranchId, classifier: C) -> Self {
        Self {
            branch,
            segmenter: Segmenter::new(classifier),
            next_ordinal: 0,
            stats: ReplayStats::default(),
            events: None,
            cwd: None,
        }
    }

    /// Also keeps a [`RecorderEvent`] for every sealed block and turn end, to be drained with
    /// [`GraphRecorder::take_events`].
    pub fn with_events(mut self) -> Self {
        self.events = Some(Vec::new());
        self
    }

    /// Returns the events kept since the last call. Empty unless built [`Self::with_events`].
    pub fn take_events(&mut self) -> Vec<RecorderEvent> {
        self.events.as_mut().map(std::mem::take).unwrap_or_default()
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
            RolloutItem::TurnContext(context) => {
                self.cwd = Some(context.cwd.to_string_lossy().into_owned());
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                let blocks = self.segmenter.flush();
                self.record(graph, blocks, &mut changes)?;
                self.push_event(RecorderEvent::TurnEnded {
                    turn_id: Some(event.turn_id.clone()),
                });
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                let blocks = self.segmenter.flush();
                self.record(graph, blocks, &mut changes)?;
                self.push_event(RecorderEvent::TurnEnded {
                    turn_id: event.turn_id.clone(),
                });
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

    fn push_event(&mut self, event: RecorderEvent) {
        if let Some(events) = &mut self.events {
            events.push(event);
        }
    }

    fn record(
        &mut self,
        graph: &mut ContextGraph,
        blocks: Vec<SealedBlock>,
        changes: &mut Vec<GraphChange>,
    ) -> Result<(), GraphError> {
        let mut moved = false;
        for SealedBlock { block, items } in blocks {
            let id = block.id.clone();
            let observed = self.events.is_some().then(|| block.clone());
            let outcome = graph.append(&self.branch, block)?;
            if let Some(block) = observed {
                self.push_event(RecorderEvent::Block(ObservedBlock {
                    block,
                    outcome,
                    items,
                    cwd: self.cwd.clone(),
                }));
            }
            match outcome {
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
