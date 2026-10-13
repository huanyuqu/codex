//! Recording saved threads that ran without the context graph, so their forks show up as
//! branches, and the merges an earlier merge tool made between them as merges.

use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context as _;
use codex_context_graph::BlockId;
use codex_context_graph::Branch;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphRecorder;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use codex_thread_store::ThreadStore;

use crate::IndexerConfig;
use crate::LocalContextGraphStore;
use crate::MergeMode;
use crate::MergeRecord;
use crate::ShadowIndexer;

/// Messages an earlier merge tool added to a merged conversation, by the text they start with,
/// and how each carried the merged branches.
const V1_MERGE_PREFIXES: [(&str, MergeMode); 4] = [
    (
        "Codex context merge (historical branch data):\n",
        MergeMode::Inline,
    ),
    (
        "Codex conversation graph merge (quoted branch history):\n",
        MergeMode::Inline,
    ),
    (
        "Codex semantic context merge (model-generated historical state):\n",
        MergeMode::Synthesize,
    ),
    (
        "Codex conversation graph synthesis (model-generated reading):\n",
        MergeMode::Synthesize,
    ),
];

/// JSON fields of those messages that name the threads a merge read.
const V1_SOURCE_FIELDS: [&str; 4] = [
    "sourceThreadIds",
    "sourceThreadId",
    "baseThreadId",
    "primarySourceThreadId",
];

/// Moves the blocks of one graph into another after the target's own, keeping the blocks the
/// target already has.
const JOIN_BLOCKS: &str =
    "UPDATE OR IGNORE cg_blocks SET graph_id = ?, seq = seq + ? WHERE graph_id = ?";

/// Moves the other rows of one graph into another, keeping the rows the target already has.
const JOIN_GRAPH: [&str; 9] = [
    "UPDATE OR IGNORE cg_branches SET graph_id = ? WHERE graph_id = ?",
    "UPDATE OR IGNORE cg_edges SET graph_id = ? WHERE graph_id = ?",
    "UPDATE OR IGNORE cg_cards SET graph_id = ? WHERE graph_id = ?",
    "UPDATE OR IGNORE cg_turn_cards SET graph_id = ? WHERE graph_id = ?",
    "UPDATE cg_index_requests SET graph_id = ? WHERE graph_id = ?",
    "UPDATE cg_views SET graph_id = ? WHERE graph_id = ?",
    "UPDATE cg_merges SET graph_id = ? WHERE graph_id = ?",
    "UPDATE OR IGNORE cg_episode_cards SET graph_id = ? WHERE graph_id = ?",
    "UPDATE OR IGNORE cg_branch_cards SET graph_id = ? WHERE graph_id = ?",
];

/// A saved thread to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedThread {
    pub thread_id: String,
    /// The thread it was forked from, if any.
    pub forked_from: Option<String>,
    /// The name the user gave the thread, used for its branch when it is a fork.
    pub name: Option<String>,
}

/// Where an import reads the items of saved threads.
pub trait SavedItems {
    fn items(
        &self,
        thread_id: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<RolloutItem>>> + Send;
}

impl SavedItems for dyn ThreadStore {
    async fn items(&self, thread_id: &str) -> anyhow::Result<Vec<RolloutItem>> {
        crate::live::saved_items(self, thread_id).await
    }
}

impl SavedItems for HashMap<String, Vec<RolloutItem>> {
    async fn items(&self, thread_id: &str) -> anyhow::Result<Vec<RolloutItem>> {
        self.get(thread_id)
            .cloned()
            .with_context(|| format!("no saved items for {thread_id}"))
    }
}

/// What an import did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Threads recorded now.
    pub recorded: usize,
    /// Threads that were recorded before.
    pub already_recorded: usize,
    /// Merges found in merged conversations and recorded.
    pub merges: usize,
    /// Threads that could not be recorded, with why.
    pub failed: Vec<(String, String)>,
}

impl LocalContextGraphStore {
    /// Records `threads` that are not recorded yet, reading their items from `saved`: parents
    /// before their forks so each fork joins its parent's graph, with extracted cards for their
    /// blocks. Then records the merges that merged conversations among them hold.
    pub async fn import_threads<S: SavedItems + ?Sized>(
        &self,
        saved: &S,
        threads: &[SavedThread],
    ) -> anyhow::Result<ImportReport> {
        let mut report = ImportReport::default();
        let mut merges: Vec<(&SavedThread, Vec<V1Merge>)> = Vec::new();
        for index in lineage_order(threads) {
            let thread = &threads[index];
            let recorded = self.branch_of_thread(&thread.thread_id).await?.is_some();
            let items = match saved.items(&thread.thread_id).await {
                Ok(items) => items,
                // A recorded thread only needs its items to find earlier merges.
                Err(_) if recorded => {
                    report.already_recorded += 1;
                    continue;
                }
                Err(err) => {
                    report
                        .failed
                        .push((thread.thread_id.clone(), format!("{err:#}")));
                    continue;
                }
            };
            merges.push((thread, v1_merges(&items)));
            if recorded {
                report.already_recorded += 1;
                continue;
            }
            match self.record_saved_thread(thread, &items).await {
                Ok(()) => report.recorded += 1,
                Err(err) => report
                    .failed
                    .push((thread.thread_id.clone(), format!("{err:#}"))),
            }
        }
        let parents: HashMap<&str, &str> = threads
            .iter()
            .filter_map(|thread| Some((thread.thread_id.as_str(), thread.forked_from.as_deref()?)))
            .collect();
        for (thread, found) in merges {
            report.merges += self.import_v1_merges(thread, found, &parents).await?;
        }
        Ok(report)
    }

    async fn record_saved_thread(
        &self,
        thread: &SavedThread,
        items: &[RolloutItem],
    ) -> anyhow::Result<()> {
        if thread.forked_from.is_some()
            && let Some(name) = &thread.name
        {
            self.name_thread_branch(&thread.thread_id, name).await?;
        }
        let membership = self
            .membership_for_thread(&thread.thread_id, thread.forked_from.as_deref())
            .await?;
        let mut graph = ContextGraph::new();
        let mut branch = Branch::new(membership.branch.clone(), membership.name.clone());
        branch.thread_id = Some(membership.thread_id.clone());
        graph.add_branch(branch)?;
        let mut recorder =
            GraphRecorder::new(membership.branch.clone(), DefaultClassifier).with_events();
        let mut events = Vec::new();
        for item in items {
            recorder.observe(&mut graph, item)?;
            events.extend(recorder.take_events());
        }
        recorder.finish(&mut graph)?;
        events.extend(recorder.take_events());

        let mut stored = graph
            .branch(&membership.branch)
            .cloned()
            .context("the replayed thread has no branch")?;
        stored.parent.clone_from(&membership.parent);
        if membership.parent.is_some()
            && let Some(parent_graph) = self.load_graph(&membership.graph_id).await?
            && let Some(head) = &stored.head
        {
            // The fork left its parent after the last block they share.
            stored.fork_point = graph
                .primary_path(head)?
                .into_iter()
                .take_while(|id| parent_graph.node(id).is_some())
                .last();
        }
        self.merge_graph(&membership.graph_id, &graph).await?;
        self.upsert_branch(&membership.graph_id, &stored).await?;

        let mut indexer = ShadowIndexer::new(
            self.clone(),
            membership.graph_id.clone(),
            membership.branch.clone(),
            /*model*/ None,
            IndexerConfig::default(),
        );
        for event in events {
            indexer.observe(event).await?;
        }
        indexer.finish().await;
        Ok(())
    }

    /// Records the merges `thread` holds as messages of an earlier merge tool, joining the
    /// graphs of the merged threads into the thread's graph.
    async fn import_v1_merges(
        &self,
        thread: &SavedThread,
        found: Vec<V1Merge>,
        parents: &HashMap<&str, &str>,
    ) -> anyhow::Result<usize> {
        if found.is_empty() {
            return Ok(0);
        }
        let Some((target_graph, target)) = self.branch_of_thread(&thread.thread_id).await? else {
            return Ok(0);
        };
        // The thread's own lineage is history it has, not something merged into it.
        let mut lineage: HashSet<&str> = HashSet::from([thread.thread_id.as_str()]);
        let mut next = parents.get(thread.thread_id.as_str()).copied();
        while let Some(parent) = next
            && lineage.insert(parent)
        {
            next = parents.get(parent).copied();
        }
        let mut recorded = 0;
        for merge in found {
            for source_thread in merge.sources {
                if lineage.contains(source_thread.as_str()) {
                    continue;
                }
                let Some((source_graph, source)) = self.branch_of_thread(&source_thread).await?
                else {
                    continue;
                };
                if source_graph != target_graph {
                    self.join_graph(&source_graph, &target_graph).await?;
                }
                let known = self
                    .load_merges(&target_graph)
                    .await?
                    .iter()
                    .any(|record| record.target == target.id && record.source == source.id);
                if known {
                    continue;
                }
                self.record_merge(
                    &target_graph,
                    &MergeRecord {
                        target: target.id.clone(),
                        source: source.id.clone(),
                        source_head: source.head.clone(),
                        target_head: None,
                        merge_block: merge.block.clone(),
                        mode: merge.mode,
                    },
                )
                .await?;
                recorded += 1;
            }
        }
        if recorded > 0 {
            // A merge block may come before the head of a branch joined after it.
            self.resequence_graph(&target_graph).await?;
        }
        Ok(recorded)
    }

    /// Renumbers the blocks of `graph_id` so every block comes after its parents, keeping the
    /// recorded order otherwise.
    async fn resequence_graph(&self, graph_id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let blocks: Vec<String> =
            sqlx::query_scalar("SELECT block_id FROM cg_blocks WHERE graph_id = ? ORDER BY seq")
                .bind(graph_id)
                .fetch_all(&mut *tx)
                .await?;
        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT child_block_id, parent_block_id FROM cg_edges WHERE graph_id = ?",
        )
        .bind(graph_id)
        .fetch_all(&mut *tx)
        .await?;
        let order = parents_first(&blocks, &edges);
        // Negative numbers first, so no two blocks share a number while they move.
        sqlx::query("UPDATE cg_blocks SET seq = -seq - 1 WHERE graph_id = ?")
            .bind(graph_id)
            .execute(&mut *tx)
            .await?;
        for (seq, block) in order.iter().enumerate() {
            sqlx::query("UPDATE cg_blocks SET seq = ? WHERE graph_id = ? AND block_id = ?")
                .bind(i64::try_from(seq)?)
                .bind(graph_id)
                .bind(block)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Moves the graph `from` into the graph `into`, so its branches become branches of `into`.
    pub async fn join_graph(&self, from: &str, into: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let offset = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM cg_blocks WHERE graph_id = ?",
        )
        .bind(into)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(JOIN_BLOCKS)
            .bind(into)
            .bind(offset)
            .bind(from)
            .execute(&mut *tx)
            .await?;
        for statement in JOIN_GRAPH {
            sqlx::query(statement)
                .bind(into)
                .bind(from)
                .execute(&mut *tx)
                .await?;
        }
        // Rows the target already had are left behind; the graph `from` is gone.
        for statement in crate::forget::DELETE_GRAPH {
            sqlx::query(statement).bind(from).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// A merge an earlier merge tool made: the threads it read and the block holding its message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct V1Merge {
    sources: Vec<String>,
    block: Option<BlockId>,
    mode: MergeMode,
}

fn v1_merges(items: &[RolloutItem]) -> Vec<V1Merge> {
    let mut merges = Vec::new();
    for item in items {
        let RolloutItem::ResponseItem(envelope) = item else {
            continue;
        };
        let ResponseItem::Message { id, content, .. } = &envelope.item else {
            continue;
        };
        let Some(text) = content.iter().find_map(|content| match content {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        }) else {
            continue;
        };
        let Some((json, mode)) = V1_MERGE_PREFIXES
            .iter()
            .find_map(|(prefix, mode)| Some((text.strip_prefix(prefix)?, *mode)))
        else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        let mut sources = Vec::new();
        collect_sources(&value, &mut sources);
        if sources.is_empty() {
            continue;
        }
        merges.push(V1Merge {
            sources,
            block: id.as_ref().map(|id| BlockId::for_item_key(id.as_str())),
            mode,
        });
    }
    merges
}

/// The thread IDs in the source fields of `value`, in order, once each.
fn collect_sources(value: &serde_json::Value, sources: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if V1_SOURCE_FIELDS.contains(&key.as_str()) {
                    let ids: Vec<&str> = match value {
                        serde_json::Value::String(id) => vec![id.as_str()],
                        serde_json::Value::Array(ids) => {
                            ids.iter().filter_map(serde_json::Value::as_str).collect()
                        }
                        _ => Vec::new(),
                    };
                    for id in ids {
                        if !sources.iter().any(|known| known == id) {
                            sources.push(id.to_string());
                        }
                    }
                } else {
                    collect_sources(value, sources);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_sources(value, sources);
            }
        }
        _ => {}
    }
}

/// `blocks` in their order, except that each comes after the parents `edges` give it
/// (child, parent).
fn parents_first(blocks: &[String], edges: &[(String, String)]) -> Vec<String> {
    let known: HashSet<&str> = blocks.iter().map(String::as_str).collect();
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (child, parent) in edges {
        if known.contains(parent.as_str()) {
            parents
                .entry(child.as_str())
                .or_default()
                .push(parent.as_str());
        }
    }
    let mut placed: HashSet<&str> = HashSet::new();
    let mut order = Vec::with_capacity(blocks.len());
    for block in blocks {
        // Depth-first over unplaced parents; the graph is acyclic.
        let mut stack = vec![(block.as_str(), false)];
        while let Some((block, expanded)) = stack.pop() {
            if placed.contains(block) {
                continue;
            }
            if expanded {
                placed.insert(block);
                order.push(block.to_string());
                continue;
            }
            stack.push((block, true));
            for parent in parents.get(block).into_iter().flatten().rev() {
                if !placed.contains(parent) {
                    stack.push((parent, false));
                }
            }
        }
    }
    order
}

/// Indexes of `threads` with every thread after the thread it was forked from.
fn lineage_order(threads: &[SavedThread]) -> Vec<usize> {
    let index: HashMap<&str, usize> = threads
        .iter()
        .enumerate()
        .map(|(position, thread)| (thread.thread_id.as_str(), position))
        .collect();
    let depth = |mut position: usize| {
        let mut depth = 0usize;
        let mut seen = HashSet::from([position]);
        while let Some(parent) = threads[position]
            .forked_from
            .as_deref()
            .and_then(|parent| index.get(parent))
            && seen.insert(*parent)
        {
            depth += 1;
            position = *parent;
        }
        depth
    };
    let mut order: Vec<usize> = (0..threads.len()).collect();
    order.sort_by_key(|position| depth(*position));
    order
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
