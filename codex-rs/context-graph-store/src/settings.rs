//! What users say about branches: what a branch is for and whether it is archived, kept by
//! thread so settings given before a thread first records still apply.

use std::collections::HashMap;

use sqlx::Row;

use crate::LocalContextGraphStore;

/// A thread's branch settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchSettings {
    /// What the branch is for, as its creator described it.
    pub purpose: Option<String>,
    /// Archived branches stay in the graph but leave the lists of branches to work on.
    pub archived: bool,
}

impl LocalContextGraphStore {
    /// Sets what `thread_id`'s branch is for; an empty purpose clears it.
    pub async fn set_branch_purpose(&self, thread_id: &str, purpose: &str) -> anyhow::Result<()> {
        let purpose = Some(purpose.trim()).filter(|purpose| !purpose.is_empty());
        sqlx::query(
            "INSERT INTO cg_thread_settings (thread_id, purpose) VALUES (?, ?) \
             ON CONFLICT(thread_id) DO UPDATE SET purpose = excluded.purpose",
        )
        .bind(thread_id)
        .bind(purpose)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Archives `thread_id`'s branch, or restores it.
    pub async fn set_branch_archived(&self, thread_id: &str, archived: bool) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_thread_settings (thread_id, archived) VALUES (?, ?) \
             ON CONFLICT(thread_id) DO UPDATE SET archived = excluded.archived",
        )
        .bind(thread_id)
        .bind(archived)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The settings of the threads whose branches are in `graph_id`, by thread.
    pub async fn load_branch_settings(
        &self,
        graph_id: &str,
    ) -> anyhow::Result<HashMap<String, BranchSettings>> {
        let rows = sqlx::query(
            "SELECT s.thread_id, s.purpose, s.archived FROM cg_thread_settings s \
             JOIN cg_branches b ON b.thread_id = s.thread_id WHERE b.graph_id = ?",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("thread_id")?,
                    BranchSettings {
                        purpose: row.try_get("purpose")?,
                        archived: row.try_get("archived")?,
                    },
                ))
            })
            .collect()
    }
}
