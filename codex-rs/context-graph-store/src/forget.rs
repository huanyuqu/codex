//! Removing what the index derived from deleted threads.

use std::collections::HashSet;

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use sqlx::Row;

use crate::LocalContextGraphStore;

/// Removes a whole graph from every table keyed by graph.
pub(crate) const DELETE_GRAPH: [&str; 10] = [
    "DELETE FROM cg_branches WHERE graph_id = ?",
    "DELETE FROM cg_blocks WHERE graph_id = ?",
    "DELETE FROM cg_edges WHERE graph_id = ?",
    "DELETE FROM cg_cards WHERE graph_id = ?",
    "DELETE FROM cg_turn_cards WHERE graph_id = ?",
    "DELETE FROM cg_index_requests WHERE graph_id = ?",
    "DELETE FROM cg_views WHERE graph_id = ?",
    "DELETE FROM cg_merges WHERE graph_id = ?",
    "DELETE FROM cg_episode_cards WHERE graph_id = ?",
    "DELETE FROM cg_branch_cards WHERE graph_id = ?",
];

impl LocalContextGraphStore {
    /// Removes what the index holds for `thread_id`: its branches, and the blocks and cards no
    /// other branch reaches, or the whole graph when no other thread's branch remains.
    pub async fn delete_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        let rows = sqlx::query("SELECT graph_id, branch_id FROM cg_branches WHERE thread_id = ?")
            .bind(thread_id)
            .fetch_all(&self.pool)
            .await?;
        let mut graphs: Vec<String> = Vec::new();
        let mut deleted: HashSet<BranchId> = HashSet::new();
        for row in &rows {
            let graph_id: String = row.try_get("graph_id")?;
            if !graphs.contains(&graph_id) {
                graphs.push(graph_id);
            }
            deleted.insert(BranchId::new(row.try_get::<String, _>("branch_id")?));
        }
        for graph_id in graphs {
            self.forget_branches(&graph_id, &deleted).await?;
        }
        sqlx::query("DELETE FROM cg_thread_names WHERE thread_id = ?")
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn forget_branches(
        &self,
        graph_id: &str,
        deleted: &HashSet<BranchId>,
    ) -> anyhow::Result<()> {
        let Some(graph) = self.load_graph(graph_id).await? else {
            return Ok(());
        };
        let remaining: Vec<_> = graph
            .branches()
            .filter(|branch| !deleted.contains(&branch.id))
            .collect();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if remaining.is_empty() {
            for statement in DELETE_GRAPH {
                sqlx::query(statement)
                    .bind(graph_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            return Ok(());
        }
        let mut kept: HashSet<BlockId> = HashSet::new();
        for branch in &remaining {
            if let Some(head) = &branch.head {
                kept.extend(graph.ancestors(head)?);
            }
        }
        for node in graph.nodes().filter(|node| !kept.contains(&node.block.id)) {
            let block = node.block.id.as_str();
            for statement in [
                "DELETE FROM cg_blocks WHERE graph_id = ? AND block_id = ?",
                "DELETE FROM cg_edges WHERE graph_id = ? AND child_block_id = ?",
                "DELETE FROM cg_cards WHERE graph_id = ? AND block_id = ?",
                "DELETE FROM cg_episode_cards WHERE graph_id = ?1 \
                 AND (first_block = ?2 OR last_block = ?2)",
            ] {
                sqlx::query(statement)
                    .bind(graph_id)
                    .bind(block)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for branch in deleted {
            for statement in [
                "DELETE FROM cg_branches WHERE graph_id = ? AND branch_id = ?",
                "DELETE FROM cg_turn_cards WHERE graph_id = ? AND branch_id = ?",
                "DELETE FROM cg_views WHERE graph_id = ? AND branch_id = ?",
                "DELETE FROM cg_branch_cards WHERE graph_id = ? AND branch_id = ?",
            ] {
                sqlx::query(statement)
                    .bind(graph_id)
                    .bind(branch.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query(
                "DELETE FROM cg_merges WHERE graph_id = ? \
                 AND (source_branch_id = ? OR target_branch_id = ?)",
            )
            .bind(graph_id)
            .bind(branch.as_str())
            .bind(branch.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "forget_tests.rs"]
mod tests;
