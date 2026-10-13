//! Turns recorder events into cards, without a model.

use codex_history::RolloutItem;
use codex_protocol::models::ResponseItem;

use crate::AppendOutcome;
use crate::BranchId;
use crate::ContextGraph;
use crate::GraphError;
use crate::GraphRecorder;
use crate::MessageClassifier;
use crate::ObservedBlock;
use crate::RecorderEvent;
use crate::ReplayStats;
use crate::card::BlockCard;
use crate::extract::extract_facts;
use crate::links::LinkTracker;
use crate::turn::TurnAccumulator;
use crate::turn::TurnCard;

/// What [`BlockIndexer::observe`] produced.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexUpdate {
    /// The extracted card of a new block, with the items a model may describe it from.
    Block {
        card: Box<BlockCard>,
        items: Vec<ResponseItem>,
    },
    /// A turn ended. Carries its card when the turn produced blocks.
    TurnEnded { card: Option<TurnCard> },
}

/// Builds extracted block cards and turn cards for one branch, in block order.
///
/// History a fork inherited is tracked so later blocks can link to it, but gets no new cards:
/// it already has them from the branch that produced it.
#[derive(Debug, Default)]
pub struct BlockIndexer {
    links: LinkTracker,
    turn: TurnAccumulator,
}

impl BlockIndexer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, event: RecorderEvent) -> Option<IndexUpdate> {
        match event {
            RecorderEvent::Block(ObservedBlock {
                block,
                outcome,
                items,
                cwd,
            }) => {
                if outcome == AppendOutcome::Reincluded {
                    return None;
                }
                let facts = extract_facts(block.kind, &items, cwd.as_deref());
                let mut card = BlockCard::extracted(&block, facts, &items);
                self.turn.push(&card);
                if outcome == AppendOutcome::Inherited {
                    self.links.record(&card);
                    return None;
                }
                self.links.link(&mut card);
                Some(IndexUpdate::Block {
                    card: Box::new(card),
                    items,
                })
            }
            RecorderEvent::TurnEnded { turn_id } => {
                self.links.end_turn();
                Some(IndexUpdate::TurnEnded {
                    card: self.turn.finish(turn_id.as_deref()),
                })
            }
        }
    }
}

/// Replays one thread's rollout onto `branch` like [`crate::replay_rollout`], passing each card
/// update to `on_update` as soon as it is known.
pub fn index_rollout<'a, C: MessageClassifier>(
    graph: &mut ContextGraph,
    branch: &BranchId,
    items: impl IntoIterator<Item = &'a RolloutItem>,
    classifier: C,
    mut on_update: impl FnMut(IndexUpdate),
) -> Result<ReplayStats, GraphError> {
    let mut recorder = GraphRecorder::new(branch.clone(), classifier).with_events();
    let mut indexer = BlockIndexer::new();
    for item in items {
        recorder.observe(graph, item)?;
        for event in recorder.take_events() {
            if let Some(update) = indexer.observe(event) {
                on_update(update);
            }
        }
    }
    recorder.finish(graph)?;
    for event in recorder.take_events() {
        if let Some(update) = indexer.observe(event) {
            on_update(update);
        }
    }
    Ok(recorder.stats())
}

#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;
