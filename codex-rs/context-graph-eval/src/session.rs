//! Recorded rollouts replayed into context graphs, with the cards the store holds for them.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
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
use codex_context_graph::extract::extract_facts;
use codex_context_graph::history::History;
use codex_context_graph::history::HistoryBlock;
use codex_context_graph_store::LocalContextGraphStore;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutRecorder;

/// A rollout replayed into a graph with one branch, named by the rollout's thread ID.
pub struct ReplayedRollout {
    pub graph_id: String,
    pub branch: BranchId,
    pub graph: ContextGraph,
    pub events: Vec<RecorderEvent>,
}

pub async fn replay(path: &Path) -> anyhow::Result<ReplayedRollout> {
    let (items, thread_id, _) = RolloutRecorder::load_rollout_items(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let graph_id = thread_id.map_or_else(
        || {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default()
        },
        |thread_id| thread_id.to_string(),
    );
    let branch_id = BranchId::for_thread(&graph_id);
    let mut graph = ContextGraph::new();
    let mut branch = Branch::new(branch_id.clone(), graph_id.clone());
    branch.thread_id = Some(graph_id.clone());
    graph.add_branch(branch)?;
    let mut recorder = GraphRecorder::new(branch_id.clone(), DefaultClassifier).with_events();
    let mut events = Vec::new();
    for item in &items {
        recorder.observe(&mut graph, item)?;
        events.extend(recorder.take_events());
    }
    recorder.finish(&mut graph)?;
    events.extend(recorder.take_events());
    Ok(ReplayedRollout {
        graph_id,
        branch: branch_id,
        graph,
        events,
    })
}

/// The history of a rollout's branch: its blocks in order, with their items and cards.
pub struct Session {
    pub graph_id: String,
    pub branch: BranchId,
    pub blocks: Vec<Block>,
    pub items: Vec<Vec<ResponseItem>>,
    pub cards: Vec<BlockCard>,
    /// Blocks that had no stored card and got an extracted one.
    pub extracted_cards: usize,
}

impl Session {
    /// Replays `path` and takes the cards of its blocks from `store`, extracting cards for
    /// blocks the store has none for.
    pub async fn load(store: &LocalContextGraphStore, path: &Path) -> anyhow::Result<Self> {
        let replayed = replay(path).await?;
        let head = replayed
            .graph
            .branch(&replayed.branch)
            .and_then(|branch| branch.head.clone());
        let order = match head {
            Some(head) => replayed.graph.primary_path(&head)?,
            None => Vec::new(),
        };
        let mut observed: HashMap<BlockId, ObservedBlock> = HashMap::new();
        for event in replayed.events {
            if let RecorderEvent::Block(block) = event
                && block.outcome != AppendOutcome::Reincluded
            {
                observed.insert(block.block.id.clone(), block);
            }
        }
        let mut stored: HashMap<BlockId, BlockCard> = store
            .load_cards(&replayed.graph_id)
            .await?
            .into_iter()
            .map(|card| (card.block_id.clone(), card))
            .collect();

        let mut session = Self {
            graph_id: replayed.graph_id,
            branch: replayed.branch,
            blocks: Vec::new(),
            items: Vec::new(),
            cards: Vec::new(),
            extracted_cards: 0,
        };
        for id in order {
            let Some(ObservedBlock {
                block, items, cwd, ..
            }) = observed.remove(&id)
            else {
                continue;
            };
            let card = stored.remove(&id).unwrap_or_else(|| {
                session.extracted_cards += 1;
                let facts = extract_facts(block.kind, &items, cwd.as_deref());
                BlockCard::extracted(&block, facts, &items)
            });
            session.blocks.push(block);
            session.items.push(items);
            session.cards.push(card);
        }
        Ok(session)
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
