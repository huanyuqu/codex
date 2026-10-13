//! Records a live thread's context graph as its rollout grows.
//!
//! The recorder observes items after they are durably appended, feeds them through the same
//! [`GraphRecorder`] that offline replay uses, and mirrors changes into the context graph store on
//! a background task. It never changes what the model sees.

use std::sync::Arc;
use std::sync::Mutex;

use codex_context_graph::BlockNode;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphChange;
use codex_context_graph::GraphRecorder;
use codex_rollout::RolloutItem;
use codex_state::SqliteConfig;
use codex_thread_store::AppendObserver;
use codex_thread_store::LiveThread;
use tokio::sync::mpsc;
use tracing::warn;

use crate::LocalContextGraphStore;

enum StoreWrite {
    Replace(Box<ContextGraph>),
    Block(Box<BlockNode>),
    Branch(Box<Branch>),
}

struct RecorderState {
    graph: ContextGraph,
    recorder: GraphRecorder<DefaultClassifier>,
}

struct GraphObserver {
    state: Mutex<RecorderState>,
    writes: mpsc::UnboundedSender<StoreWrite>,
}

impl AppendObserver for GraphObserver {
    fn observe(&self, items: &[RolloutItem]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let RecorderState { graph, recorder } = &mut *state;
        for item in items {
            match recorder.observe(graph, item) {
                Ok(changes) => send_changes(graph, changes, &self.writes),
                Err(err) => warn!("context graph recording failed: {err}"),
            }
        }
    }
}

fn send_changes(
    graph: &ContextGraph,
    changes: Vec<GraphChange>,
    writes: &mpsc::UnboundedSender<StoreWrite>,
) {
    for change in changes {
        let write = match change {
            GraphChange::BlockAdded(id) => graph
                .node(&id)
                .map(|node| StoreWrite::Block(Box::new(node.clone()))),
            GraphChange::BranchUpdated(id) => graph
                .branch(&id)
                .map(|branch| StoreWrite::Branch(Box::new(branch.clone()))),
        };
        if let Some(write) = write {
            // The writer only stops when the session ends; later changes have nowhere to go.
            let _ = writes.send(write);
        }
    }
}

/// Starts recording `live_thread` into the context graph store under `graph_id`.
///
/// `initial_items` is the history the thread starts from, such as a resumed rollout, so the stored
/// graph covers the whole thread rather than only what this process appends. Returns false when
/// recording could not start; the thread itself is unaffected either way.
pub fn install_live_recorder(
    live_thread: &LiveThread,
    graph_id: &str,
    initial_items: &[RolloutItem],
    sqlite: SqliteConfig,
) -> bool {
    let graph_id = graph_id.to_string();
    let branch_id = BranchId::for_thread(&graph_id);
    let mut graph = ContextGraph::new();
    let mut branch = Branch::new(branch_id.clone(), graph_id.clone());
    branch.thread_id = Some(graph_id.clone());
    if let Err(err) = graph.add_branch(branch) {
        warn!("context graph recording disabled: {err}");
        return false;
    }
    let mut recorder = GraphRecorder::new(branch_id, DefaultClassifier);
    for item in initial_items {
        if let Err(err) = recorder.observe(&mut graph, item) {
            warn!("context graph recording disabled: {err}");
            return false;
        }
    }

    let (writes, receiver) = mpsc::unbounded_channel();
    let _ = writes.send(StoreWrite::Replace(Box::new(graph.clone())));
    tokio::spawn(write_to_store(sqlite, graph_id, receiver));

    let observer = GraphObserver {
        state: Mutex::new(RecorderState { graph, recorder }),
        writes,
    };
    let installed = live_thread.set_append_observer(Arc::new(observer));
    if !installed {
        warn!("context graph recording skipped: thread already has an append observer");
    }
    installed
}

async fn write_to_store(
    sqlite: SqliteConfig,
    graph_id: String,
    mut receiver: mpsc::UnboundedReceiver<StoreWrite>,
) {
    let store = match LocalContextGraphStore::open(&sqlite).await {
        Ok(store) => store,
        Err(err) => {
            warn!("context graph store unavailable: {err:#}");
            return;
        }
    };
    while let Some(write) = receiver.recv().await {
        let result = match write {
            StoreWrite::Replace(graph) => store.replace_graph(&graph_id, &graph).await,
            StoreWrite::Block(node) => store.append_block(&graph_id, &node).await,
            StoreWrite::Branch(branch) => store.upsert_branch(&graph_id, &branch).await,
        };
        if let Err(err) = result {
            warn!("context graph store write failed: {err:#}");
        }
    }
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
