//! Records a live thread's context graph as its rollout grows.
//!
//! The recorder observes items after they are durably appended, feeds them through the same
//! [`GraphRecorder`] that offline replay uses, and mirrors changes into the context graph store on
//! a background task. With indexing enabled, a [`ShadowIndexer`] on another task writes the cards
//! of new blocks. The returned [`LiveContextGraph`] gives recall the branch's blocks with their
//! items, current as of the last append.
//!
//! A forked thread joins the graph of the thread it was forked from as a new branch. Each thread
//! keeps only its own branch in memory, and the store merges the branches, so recording never
//! discards what another thread stored.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use codex_context_graph::AppendOutcome;
use codex_context_graph::BlockId;
use codex_context_graph::BlockNode;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphChange;
use codex_context_graph::GraphRecorder;
use codex_context_graph::ObservedBlock;
use codex_context_graph::RecorderEvent;
use codex_rollout::RolloutItem;
use codex_state::SqliteConfig;
use codex_thread_store::AppendObserver;
use codex_thread_store::LiveThread;
use tokio::sync::mpsc;
use tracing::warn;

use crate::LocalContextGraphStore;
use crate::indexer::IndexerConfig;
use crate::indexer::ShadowIndexer;
use crate::model::ShadowModel;
use crate::snapshot::BranchSnapshot;

/// The graph a thread records into and its branch there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphMembership {
    pub graph_id: String,
    pub branch: BranchId,
    pub name: String,
    pub thread_id: String,
    /// The branch this thread was forked from. The history the thread starts with is inherited
    /// from it, and its last block is the fork point.
    pub parent: Option<BranchId>,
}

impl GraphMembership {
    /// A thread that starts its own graph, named by its thread ID.
    pub fn root(thread_id: &str) -> Self {
        Self {
            graph_id: thread_id.to_string(),
            branch: BranchId::for_thread(thread_id),
            name: "main".to_string(),
            thread_id: thread_id.to_string(),
            parent: None,
        }
    }

    /// A thread forked from `parent`, which joins its graph as a new branch.
    pub fn fork(
        thread_id: &str,
        graph_id: &str,
        parent: BranchId,
        name: impl Into<String>,
    ) -> Self {
        Self {
            graph_id: graph_id.to_string(),
            branch: BranchId::for_thread(thread_id),
            name: name.into(),
            thread_id: thread_id.to_string(),
            parent: Some(parent),
        }
    }
}

/// How a live recorder indexes the blocks it records.
#[derive(Clone, Default)]
pub struct LiveIndexing {
    /// Model that describes blocks; without one, blocks only get extracted cards.
    pub model: Option<Arc<dyn ShadowModel>>,
    pub config: IndexerConfig,
}

enum StoreWrite {
    Merge(Box<ContextGraph>),
    Block(Box<BlockNode>),
    Branch(Box<Branch>),
}

struct RecorderState {
    graph: ContextGraph,
    recorder: GraphRecorder<DefaultClassifier>,
    /// Every block recorded on the branch, with the items it was cut from.
    observed: HashMap<BlockId, ObservedBlock>,
    /// Where the branch left its parent, once the inherited history is recorded.
    fork_point: Option<BlockId>,
}

impl RecorderState {
    /// Keeps the items of new blocks and passes every event on to the indexer, if any.
    fn take_events(&mut self, events: Option<&mpsc::UnboundedSender<RecorderEvent>>) {
        for event in self.recorder.take_events() {
            if let RecorderEvent::Block(block) = &event
                && block.outcome != AppendOutcome::Reincluded
            {
                self.observed.insert(block.block.id.clone(), block.clone());
            }
            if let Some(events) = events {
                // The indexer only stops when the session ends; later events have nowhere to go.
                let _ = events.send(event);
            }
        }
    }

    /// The branch as stored: with its parent and fork point, which the in-memory graph of a
    /// single thread cannot hold.
    fn stored_branch(&self, membership: &GraphMembership) -> Option<Branch> {
        let mut branch = self.graph.branch(&membership.branch)?.clone();
        branch.parent.clone_from(&membership.parent);
        branch.fork_point.clone_from(&self.fork_point);
        Some(branch)
    }
}

struct GraphObserver {
    membership: GraphMembership,
    state: Arc<Mutex<RecorderState>>,
    writes: mpsc::UnboundedSender<StoreWrite>,
    events: Option<mpsc::UnboundedSender<RecorderEvent>>,
}

impl AppendObserver for GraphObserver {
    fn observe(&self, items: &[RolloutItem]) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        for item in items {
            let RecorderState {
                graph, recorder, ..
            } = &mut *state;
            match recorder.observe(graph, item) {
                Ok(changes) => self.send_changes(&state, changes),
                Err(err) => warn!("context graph recording failed: {err}"),
            }
            state.take_events(self.events.as_ref());
        }
    }
}

impl GraphObserver {
    fn send_changes(&self, state: &RecorderState, changes: Vec<GraphChange>) {
        for change in changes {
            let write = match change {
                GraphChange::BlockAdded(id) => state
                    .graph
                    .node(&id)
                    .map(|node| StoreWrite::Block(Box::new(node.clone()))),
                GraphChange::BranchUpdated(id) if id == self.membership.branch => state
                    .stored_branch(&self.membership)
                    .map(|branch| StoreWrite::Branch(Box::new(branch))),
                // Rolled-back history keeps its own branch.
                GraphChange::BranchUpdated(id) => state
                    .graph
                    .branch(&id)
                    .map(|branch| StoreWrite::Branch(Box::new(branch.clone()))),
            };
            if let Some(write) = write {
                // The writer only stops when the session ends; later changes have nowhere to go.
                let _ = self.writes.send(write);
            }
        }
    }
}

impl LocalContextGraphStore {
    /// Where `thread_id` records: its stored branch when it has one, such as a resumed thread; a
    /// new branch of its parent's graph when it was forked from a recorded thread; otherwise a
    /// graph of its own.
    pub async fn membership_for_thread(
        &self,
        thread_id: &str,
        forked_from: Option<&str>,
    ) -> anyhow::Result<GraphMembership> {
        if let Some((graph_id, branch)) = self.branch_of_thread(thread_id).await? {
            return Ok(GraphMembership {
                graph_id,
                branch: branch.id,
                name: branch.name,
                thread_id: thread_id.to_string(),
                parent: branch.parent,
            });
        }
        if let Some(parent) = forked_from
            && let Some((graph_id, parent_branch)) = self.branch_of_thread(parent).await?
        {
            // Thread IDs end in random digits, so their tail tells forks apart.
            let short = thread_id
                .char_indices()
                .rev()
                .nth(7)
                .map_or(thread_id, |(start, _)| &thread_id[start..]);
            return Ok(GraphMembership::fork(
                thread_id,
                &graph_id,
                parent_branch.id,
                format!("fork-{short}"),
            ));
        }
        Ok(GraphMembership::root(thread_id))
    }
}

/// A live thread's branch, for recall.
pub struct LiveContextGraph {
    membership: GraphMembership,
    sqlite: SqliteConfig,
    state: Arc<Mutex<RecorderState>>,
}

impl LiveContextGraph {
    pub fn membership(&self) -> &GraphMembership {
        &self.membership
    }

    /// The blocks of the branch in history order, with their items, including everything the
    /// thread appended so far.
    pub fn observed_history(&self) -> Vec<ObservedBlock> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        let Some(head) = state
            .graph
            .branch(&self.membership.branch)
            .and_then(|branch| branch.head.clone())
        else {
            return Vec::new();
        };
        let path = state.graph.primary_path(&head).unwrap_or_default();
        path.iter()
            .filter_map(|id| state.observed.get(id).cloned())
            .collect()
    }

    pub async fn store(&self) -> anyhow::Result<LocalContextGraphStore> {
        LocalContextGraphStore::open(&self.sqlite).await
    }

    /// The branch's history with the cards stored so far.
    pub async fn snapshot(&self) -> anyhow::Result<BranchSnapshot> {
        let observed = self.observed_history();
        let cards = self
            .store()
            .await?
            .load_cards(&self.membership.graph_id)
            .await
            .context("failed to load context graph cards")?
            .into_iter()
            .map(|card| (card.block_id.clone(), card))
            .collect();
        Ok(BranchSnapshot::new(
            self.membership.graph_id.clone(),
            self.membership.branch.clone(),
            observed,
            cards,
        ))
    }
}

/// Starts recording `live_thread` into the context graph store as `membership` says.
///
/// `initial_items` is the history the thread starts from, such as a resumed rollout or the
/// history a fork inherited, so the stored graph covers the whole thread rather than only what
/// this process appends. With `indexing`, new blocks also get cards. Returns `None` when
/// recording could not start; the thread itself is unaffected either way.
pub fn install_live_recorder(
    live_thread: &LiveThread,
    membership: GraphMembership,
    initial_items: &[RolloutItem],
    sqlite: SqliteConfig,
    indexing: Option<LiveIndexing>,
) -> Option<Arc<LiveContextGraph>> {
    let mut graph = ContextGraph::new();
    let mut branch = Branch::new(membership.branch.clone(), membership.name.clone());
    branch.thread_id = Some(membership.thread_id.clone());
    if let Err(err) = graph.add_branch(branch) {
        warn!("context graph recording disabled: {err}");
        return None;
    }
    let (events, indexer) = match indexing {
        Some(indexing) => {
            let (events, receiver) = mpsc::unbounded_channel();
            (Some(events), Some((indexing, receiver)))
        }
        None => (None, None),
    };
    let mut state = RecorderState {
        graph,
        recorder: GraphRecorder::new(membership.branch.clone(), DefaultClassifier).with_events(),
        observed: HashMap::new(),
        fork_point: None,
    };
    for item in initial_items {
        let RecorderState {
            graph, recorder, ..
        } = &mut state;
        if let Err(err) = recorder.observe(graph, item) {
            warn!("context graph recording disabled: {err}");
            return None;
        }
        state.take_events(events.as_ref());
    }
    if membership.parent.is_some() {
        state.fork_point = state
            .graph
            .branch(&membership.branch)
            .and_then(|branch| branch.head.clone());
    }

    let (writes, receiver) = mpsc::unbounded_channel();
    let _ = writes.send(StoreWrite::Merge(Box::new(state.graph.clone())));
    // The in-memory graph cannot hold the parent branch, so the stored branch gets its parent
    // and fork point separately.
    if let Some(branch) = state.stored_branch(&membership) {
        let _ = writes.send(StoreWrite::Branch(Box::new(branch)));
    }
    tokio::spawn(run_in_background(
        sqlite.clone(),
        membership.graph_id.clone(),
        membership.branch.clone(),
        receiver,
        indexer,
    ));

    let state = Arc::new(Mutex::new(state));
    let observer = GraphObserver {
        membership: membership.clone(),
        state: Arc::clone(&state),
        writes,
        events,
    };
    if !live_thread.set_append_observer(Arc::new(observer)) {
        warn!("context graph recording skipped: thread already has an append observer");
        return None;
    }
    Some(Arc::new(LiveContextGraph {
        membership,
        sqlite,
        state,
    }))
}

/// Opens the store once and runs the graph writer and, when enabled, the indexer on it.
async fn run_in_background(
    sqlite: SqliteConfig,
    graph_id: String,
    branch: BranchId,
    writes: mpsc::UnboundedReceiver<StoreWrite>,
    indexer: Option<(LiveIndexing, mpsc::UnboundedReceiver<RecorderEvent>)>,
) {
    let store = match LocalContextGraphStore::open(&sqlite).await {
        Ok(store) => store,
        Err(err) => {
            warn!("context graph store unavailable: {err:#}");
            return;
        }
    };
    let writer = write_to_store(store.clone(), graph_id.clone(), writes);
    match indexer {
        Some((indexing, events)) => {
            let indexer =
                ShadowIndexer::new(store, graph_id, branch, indexing.model, indexing.config);
            tokio::join!(writer, indexer.run(events));
        }
        None => writer.await,
    }
}

async fn write_to_store(
    store: LocalContextGraphStore,
    graph_id: String,
    mut receiver: mpsc::UnboundedReceiver<StoreWrite>,
) {
    while let Some(write) = receiver.recv().await {
        let result = match write {
            StoreWrite::Merge(graph) => store.merge_graph(&graph_id, &graph).await,
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
