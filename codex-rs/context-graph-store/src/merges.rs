//! Recorded merges between branches.

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use sqlx::Row;

use crate::LocalContextGraphStore;
use crate::cards::now;

/// A branch merged into another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRecord {
    pub target: BranchId,
    pub source: BranchId,
    /// Head of the source branch when it was merged.
    pub source_head: Option<BlockId>,
    pub mode: String,
}

impl LocalContextGraphStore {
    pub async fn record_merge(&self, graph_id: &str, merge: &MergeRecord) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_merges (graph_id, target_branch_id, source_branch_id, source_head, \
             mode, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(merge.target.as_str())
        .bind(merge.source.as_str())
        .bind(merge.source_head.as_ref().map(BlockId::as_str))
        .bind(&merge.mode)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Merges recorded in `graph_id`, oldest first.
    pub async fn load_merges(&self, graph_id: &str) -> anyhow::Result<Vec<MergeRecord>> {
        let rows = sqlx::query(
            "SELECT target_branch_id, source_branch_id, source_head, mode FROM cg_merges \
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
                    mode: row.try_get("mode")?,
                })
            })
            .collect()
    }
}
