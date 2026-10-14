//! Signals about how well recall chose: blocks the model had to read with `context_expand`
//! although the view left them out, and recalls users asked for.

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use sqlx::Row;

use crate::LocalContextGraphStore;
use crate::cards::now;

/// A signal about recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallFeedback {
    /// The model read a block the recalled window held.
    Expanded,
    /// The model read a block the recalled window left out: recall missed it.
    Missed,
    /// The user asked for a fresh recall.
    Requested,
}

impl RecallFeedback {
    fn as_str(self) -> &'static str {
        match self {
            Self::Expanded => "expanded",
            Self::Missed => "missed",
            Self::Requested => "requested",
        }
    }
}

/// Counts of the recall signals of one branch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecallFeedbackCounts {
    pub expanded: u32,
    pub missed: u32,
    pub requested: u32,
}

impl LocalContextGraphStore {
    pub async fn record_recall_feedback(
        &self,
        graph_id: &str,
        branch: &BranchId,
        feedback: RecallFeedback,
        block: Option<&BlockId>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_recall_feedback (graph_id, branch_id, kind, block_id, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(branch.as_str())
        .bind(feedback.as_str())
        .bind(block.map(BlockId::as_str))
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recall_feedback_counts(
        &self,
        graph_id: &str,
        branch: &BranchId,
    ) -> anyhow::Result<RecallFeedbackCounts> {
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS count FROM cg_recall_feedback \
             WHERE graph_id = ? AND branch_id = ? GROUP BY kind",
        )
        .bind(graph_id)
        .bind(branch.as_str())
        .fetch_all(&self.pool)
        .await?;
        let mut counts = RecallFeedbackCounts::default();
        for row in rows {
            let count = u32::try_from(row.try_get::<i64, _>("count")?).unwrap_or(u32::MAX);
            match row.try_get::<String, _>("kind")?.as_str() {
                "expanded" => counts.expanded = count,
                "missed" => counts.missed = count,
                "requested" => counts.requested = count,
                _ => {}
            }
        }
        Ok(counts)
    }
}
