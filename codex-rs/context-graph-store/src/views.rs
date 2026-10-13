//! Recorded context views.

use codex_context_graph::BranchId;
use codex_context_graph::view::ContextView;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;

use crate::LocalContextGraphStore;
use crate::cards::now;
use crate::recall::RecallStats;

/// A view recall built for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRecord {
    pub branch_id: String,
    pub question: String,
    pub selector: String,
    pub view: ContextView,
    pub stats: RecallStats,
}

impl LocalContextGraphStore {
    pub async fn record_view(
        &self,
        graph_id: &str,
        branch: &BranchId,
        question: &str,
        selector: &str,
        view: &ContextView,
        stats: &RecallStats,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_views (graph_id, branch_id, question, selector, view, stats, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(branch.as_str())
        .bind(question)
        .bind(selector)
        .bind(serde_json::to_string(view)?)
        .bind(serde_json::to_string(stats)?)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Views recorded for `graph_id`, oldest first.
    pub async fn load_views(&self, graph_id: &str) -> anyhow::Result<Vec<ViewRecord>> {
        let rows = sqlx::query(
            "SELECT branch_id, question, selector, view, stats FROM cg_views \
             WHERE graph_id = ? ORDER BY id",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(ViewRecord {
                    branch_id: row.try_get("branch_id")?,
                    question: row.try_get("question")?,
                    selector: row.try_get("selector")?,
                    view: serde_json::from_str(&row.try_get::<String, _>("view")?)?,
                    stats: serde_json::from_str(&row.try_get::<String, _>("stats")?)?,
                })
            })
            .collect()
    }
}
