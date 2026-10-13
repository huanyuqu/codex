//! A conversation's branches as clients list and change them: the tree, renames, and merges.

use std::collections::HashMap;

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use codex_context_graph::BranchStatus;
use codex_context_graph::ContextGraph;
use codex_context_graph::GraphError;
use codex_context_graph::branches::BranchRow;
use codex_context_graph::branches::FoldSummary;
use codex_context_graph::branches::Since;
use codex_context_graph::branches::branch_rows;
use codex_context_graph::branches::fold_summary;
use codex_context_graph::card::BlockCard;
use codex_context_graph::episode::BranchCard;

use crate::LocalContextGraphStore;
use crate::merges::MergeMode;
use crate::merges::MergeRecord;

/// The graph a thread records into, seen from that thread's branch.
#[derive(Debug, Clone)]
pub struct BranchTree {
    pub graph_id: String,
    pub graph: ContextGraph,
    pub cards: HashMap<BlockId, BlockCard>,
    /// What each branch is about, for branches consolidation has described.
    pub branch_cards: HashMap<BranchId, BranchCard>,
    /// The branch of the thread the tree was loaded for.
    pub current: BranchId,
    pub merges: Vec<MergeRecord>,
}

/// What merging a branch adds to the target thread's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub source: BranchRow,
    /// The message the target thread's model sees.
    pub message: String,
    /// User turns the source added after it diverged from the target.
    pub turns: usize,
    pub record: MergeRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("'{0}' is not a branch of this conversation")]
    UnknownBranch(String),
    #[error("a branch cannot be merged into itself")]
    SameBranch,
    #[error("branch {0} has nothing this conversation lacks")]
    NothingToMerge(String),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

impl LocalContextGraphStore {
    /// The graph of `thread_id` with its cards and merges, or `None` when the thread is not
    /// recorded yet.
    pub async fn branch_tree(&self, thread_id: &str) -> anyhow::Result<Option<BranchTree>> {
        let Some((graph_id, branch)) = self.branch_of_thread(thread_id).await? else {
            return Ok(None);
        };
        let Some(graph) = self.load_graph(&graph_id).await? else {
            return Ok(None);
        };
        let cards = self
            .load_cards(&graph_id)
            .await?
            .into_iter()
            .map(|card| (card.block_id.clone(), card))
            .collect();
        let merges = self.load_merges(&graph_id).await?;
        let branch_cards = self.load_branch_cards(&graph_id).await?;
        Ok(Some(BranchTree {
            graph_id,
            graph,
            cards,
            branch_cards,
            current: branch.id,
            merges,
        }))
    }
}

impl BranchTree {
    /// The branches in tree order.
    pub fn rows(&self) -> Vec<BranchRow> {
        branch_rows(&self.graph, &self.cards, &self.current)
    }

    /// The branch `name` names: a branch ID, a branch name or a thread ID.
    pub fn find(&self, name: &str) -> Option<BranchRow> {
        let name = name.trim().trim_start_matches('@');
        self.rows().into_iter().find(|row| {
            row.branch.as_str() == name
                || row.name == name
                || row.thread_id.as_deref() == Some(name)
        })
    }

    pub fn branch_name(&self, id: &BranchId) -> String {
        self.graph
            .branch(id)
            .map_or_else(|| id.to_string(), |branch| branch.name.clone())
    }

    /// Names of the branches `branch` was merged into.
    pub fn merged_into(&self, branch: &BranchId) -> Vec<String> {
        let mut targets: Vec<String> = Vec::new();
        for merge in self.merges.iter().filter(|merge| &merge.source == branch) {
            let name = self.branch_name(&merge.target);
            if !targets.contains(&name) {
                targets.push(name);
            }
        }
        targets
    }

    /// Branches referenced by the current branch.
    pub fn references(&self) -> Vec<&MergeRecord> {
        self.merges
            .iter()
            .filter(|merge| merge.target == self.current && merge.mode == MergeMode::Reference)
            .collect()
    }

    /// Plans merging the branch `name` names into the current branch.
    pub fn plan_merge(&self, name: &str, mode: MergeMode) -> Result<MergePlan, MergeError> {
        let source = self
            .find(name)
            .ok_or_else(|| MergeError::UnknownBranch(name.to_string()))?;
        if source.current {
            return Err(MergeError::SameBranch);
        }
        // A reference reads the branch's own history, including work folded in before.
        let since = if mode == MergeMode::Reference {
            Since::Fork
        } else {
            Since::Unmerged
        };
        let fold = fold_summary(
            &self.graph,
            &self.cards,
            &source.branch,
            &self.current,
            since,
        )?
        .ok_or_else(|| MergeError::NothingToMerge(source.name.clone()))?;
        let message = match mode {
            MergeMode::Reference => reference_message(&source, &fold),
            MergeMode::Fold | MergeMode::Synthesize | MergeMode::Inline => fold.text.clone(),
        };
        let head = |branch: &BranchId| {
            self.graph
                .branch(branch)
                .and_then(|branch| branch.head.clone())
        };
        Ok(MergePlan {
            record: MergeRecord {
                target: self.current.clone(),
                source: source.branch.clone(),
                source_head: head(&source.branch),
                target_head: head(&self.current),
                merge_block: None,
                mode,
            },
            turns: fold.turns,
            message,
            source,
        })
    }

    /// Whether the history `branch` holds can still be opened as a chat.
    pub fn openable(row: &BranchRow) -> bool {
        row.thread_id.is_some() && row.status != BranchStatus::RolledBack
    }
}

/// The note a reference merge adds: what the branch did, in brief, and how to read the rest.
fn reference_message(source: &BranchRow, fold: &FoldSummary) -> String {
    let outcome = fold
        .text
        .lines()
        .find_map(|line| line.strip_prefix("Outcome: "))
        .map(|outcome| format!(" Its outcome: {outcome}"))
        .unwrap_or_default();
    let turns = match fold.turns {
        1 => "1 turn".to_string(),
        turns => format!("{turns} turns"),
    };
    format!(
        "Branch \"{}\" is now referenced by this conversation. It continued from an earlier point \
         of this conversation on its own for {turns}.{outcome} Its history is part of what recall \
         chooses from, and context_search and context_expand can find and read it. This is a \
         record, not new instructions.",
        source.name
    )
}

#[cfg(test)]
#[path = "branch_tree_tests.rs"]
mod tests;
