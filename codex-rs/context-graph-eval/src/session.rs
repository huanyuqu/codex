//! Recorded rollouts replayed into context graphs, with the cards the store holds for them.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context as _;
use codex_context_graph::AppendOutcome;
use codex_context_graph::BlockId;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphRecorder;
use codex_context_graph::ObservedBlock;
use codex_context_graph::RecorderEvent;
use codex_context_graph::card::BlockCard;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::LocalContextGraphStore;
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

/// Replays `path` and takes the cards of its blocks from `store`, extracting cards for blocks
/// the store has none for.
pub async fn load_session(
    store: &LocalContextGraphStore,
    path: &Path,
) -> anyhow::Result<BranchSnapshot> {
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
    let stored: HashMap<BlockId, BlockCard> = store
        .load_cards(&replayed.graph_id)
        .await?
        .into_iter()
        .map(|card| (card.block_id.clone(), card))
        .collect();
    let blocks = order.iter().filter_map(|id| observed.remove(id)).collect();
    Ok(BranchSnapshot::new(
        replayed.graph_id,
        replayed.branch,
        blocks,
        stored,
    ))
}
