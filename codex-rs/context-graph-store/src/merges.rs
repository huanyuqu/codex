//! Recorded merges between branches.

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

use crate::LocalContextGraphStore;
use crate::cards::now;

/// How a branch was merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeMode {
    /// A summary of the branch's work entered the target's history.
    Fold,
    /// The branch's blocks became part of what the target's recall chooses from.
    Reference,
    /// A model wrote a merge note from the branch's history.
    Synthesize,
    /// The branch's history was copied into the target's.
    Inline,
}

impl MergeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fold => "fold",
            Self::Reference => "reference",
            Self::Synthesize => "synthesize",
            Self::Inline => "inline",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "reference" => Self::Reference,
            "synthesize" => Self::Synthesize,
            "inline" => Self::Inline,
            _ => Self::Fold,
        }
    }
}

/// A branch merged into another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRecord {
    pub target: BranchId,
    pub source: BranchId,
    /// Head of the source branch when it was merged.
    pub source_head: Option<BlockId>,
    /// Head of the target branch when the source was merged into it.
    pub target_head: Option<BlockId>,
    /// The block of the target branch that holds the merge note.
    pub merge_block: Option<BlockId>,
    pub mode: MergeMode,
    /// What the user wanted from the merge.
    pub goal: Option<String>,
    /// The code merged along with the branch's context.
    pub code: Option<CodeMerge>,
}

/// Code merged from a branch's worktree into the target's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMerge {
    /// The git branch merged.
    pub git_branch: String,
    /// The commit that recorded the branch's uncommitted work before the merge, if there was
    /// any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    /// The commit the merge created, unless it stopped at conflicts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit: Option<String>,
    /// Files left with conflicts for the user or the model to resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Files the merge changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

impl LocalContextGraphStore {
    /// Records `merge`. The merge block, once recorded, gets the source head as its second
    /// parent, so the graph shows where the branches joined.
    pub async fn record_merge(&self, graph_id: &str, merge: &MergeRecord) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO cg_merges (graph_id, target_branch_id, source_branch_id, source_head, \
             target_head, merge_block, mode, goal, code, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(merge.target.as_str())
        .bind(merge.source.as_str())
        .bind(merge.source_head.as_ref().map(BlockId::as_str))
        .bind(merge.target_head.as_ref().map(BlockId::as_str))
        .bind(merge.merge_block.as_ref().map(BlockId::as_str))
        .bind(merge.mode.as_str())
        .bind(merge.goal.as_deref())
        .bind(merge.code.as_ref().map(serde_json::to_string).transpose()?)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        if let (Some(block), Some(source_head)) = (&merge.merge_block, &merge.source_head) {
            // A merge of several branches gives its block one more parent per branch.
            sqlx::query(
                "INSERT OR IGNORE INTO cg_edges (graph_id, child_block_id, position, \
                 parent_block_id) SELECT ?1, ?2, (SELECT COALESCE(MAX(position) + 1, 1) \
                 FROM cg_edges WHERE graph_id = ?1 AND child_block_id = ?2), ?3 \
                 WHERE NOT EXISTS (SELECT 1 FROM cg_edges WHERE graph_id = ?1 \
                 AND child_block_id = ?2 AND parent_block_id = ?3)",
            )
            .bind(graph_id)
            .bind(block.as_str())
            .bind(source_head.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Merges recorded in `graph_id`, oldest first.
    pub async fn load_merges(&self, graph_id: &str) -> anyhow::Result<Vec<MergeRecord>> {
        let rows = sqlx::query(
            "SELECT target_branch_id, source_branch_id, source_head, target_head, merge_block, \
             mode, goal, code FROM cg_merges \
             WHERE graph_id = ? ORDER BY id",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(MergeRecord {
                    target: BranchId::new(row.try_get::<String, _>("target_branch_id")?),
                    source: BranchId::new(row.try_get::<String, _>("source_branch_id")?),
                    source_head: row
                        .try_get::<Option<String>, _>("source_head")?
                        .map(BlockId::from_stored),
                    target_head: row
                        .try_get::<Option<String>, _>("target_head")?
                        .map(BlockId::from_stored),
                    merge_block: row
                        .try_get::<Option<String>, _>("merge_block")?
                        .map(BlockId::from_stored),
                    mode: MergeMode::parse(&row.try_get::<String, _>("mode")?),
                    goal: row.try_get("goal")?,
                    code: row
                        .try_get::<Option<String>, _>("code")?
                        .map(|code| serde_json::from_str(&code))
                        .transpose()?,
                })
            })
            .collect()
    }
}
