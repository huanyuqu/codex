//! Stored index cards and indexing costs.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::EXTRACTED_INDEXER;
use codex_context_graph::turn::TurnCard;
use serde::Serialize;
use sqlx::Row;

use crate::LocalContextGraphStore;
use crate::to_i64;

/// Token counts and latency of one indexing model request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexRequest {
    pub indexer: String,
    pub blocks: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    /// Why the request produced no cards, when it failed.
    pub error: Option<String>,
}

/// Totals over the indexing requests of a graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct IndexUsage {
    pub requests: u64,
    pub failed_requests: u64,
    pub blocks: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms_total: u64,
    pub latency_ms_max: u64,
}

impl LocalContextGraphStore {
    /// Stores `card`, unless the stored card came from a model and `card` is an extracted card
    /// of the same or an older version. Replaying history therefore never discards model work.
    pub async fn upsert_card(&self, graph_id: &str, card: &BlockCard) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_cards (graph_id, block_id, card_version, indexer, card, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(graph_id, block_id) DO UPDATE SET \
             card_version = excluded.card_version, indexer = excluded.indexer, \
             card = excluded.card, updated_at = excluded.updated_at \
             WHERE excluded.card_version > cg_cards.card_version \
             OR excluded.indexer != ? OR cg_cards.indexer = ?",
        )
        .bind(graph_id)
        .bind(card.block_id.as_str())
        .bind(i64::from(card.card_version))
        .bind(&card.indexer)
        .bind(serde_json::to_string(card)?)
        .bind(now())
        .bind(EXTRACTED_INDEXER)
        .bind(EXTRACTED_INDEXER)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cards of `graph_id` in block recording order, or in the order they were first written
    /// for blocks the graph does not store.
    pub async fn load_cards(&self, graph_id: &str) -> anyhow::Result<Vec<BlockCard>> {
        let rows = sqlx::query(
            "SELECT c.card FROM cg_cards c \
             LEFT JOIN cg_blocks b ON b.graph_id = c.graph_id AND b.block_id = c.block_id \
             WHERE c.graph_id = ? ORDER BY b.seq IS NULL, b.seq, c.rowid",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(&row.try_get::<String, _>("card")?)?))
            .collect()
    }

    /// Whether `block_id` already has a card of the current version written by a model.
    pub async fn has_model_card(
        &self,
        graph_id: &str,
        block_id: &BlockId,
        card_version: u32,
    ) -> anyhow::Result<bool> {
        let found = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM cg_cards WHERE graph_id = ? AND block_id = ? \
             AND card_version >= ? AND indexer != ?",
        )
        .bind(graph_id)
        .bind(block_id.as_str())
        .bind(i64::from(card_version))
        .bind(EXTRACTED_INDEXER)
        .fetch_one(&self.pool)
        .await?;
        Ok(found > 0)
    }

    pub async fn upsert_turn_card(
        &self,
        graph_id: &str,
        branch: &BranchId,
        card: &TurnCard,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_turn_cards (graph_id, turn_id, branch_id, indexer, card, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(graph_id, turn_id) DO UPDATE SET branch_id = excluded.branch_id, \
             indexer = excluded.indexer, card = excluded.card, updated_at = excluded.updated_at",
        )
        .bind(graph_id)
        .bind(&card.turn_id)
        .bind(branch.as_str())
        .bind(&card.indexer)
        .bind(serde_json::to_string(card)?)
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load_turn_cards(&self, graph_id: &str) -> anyhow::Result<Vec<TurnCard>> {
        let rows = sqlx::query("SELECT card FROM cg_turn_cards WHERE graph_id = ? ORDER BY rowid")
            .bind(graph_id)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| Ok(serde_json::from_str(&row.try_get::<String, _>("card")?)?))
            .collect()
    }

    pub async fn record_index_request(
        &self,
        graph_id: &str,
        request: &IndexRequest,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO cg_index_requests (graph_id, indexer, blocks, input_tokens, \
             cached_input_tokens, output_tokens, latency_ms, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(&request.indexer)
        .bind(i64::from(request.blocks))
        .bind(to_i64(request.input_tokens))
        .bind(to_i64(request.cached_input_tokens))
        .bind(to_i64(request.output_tokens))
        .bind(to_i64(request.latency_ms))
        .bind(request.error.as_deref())
        .bind(now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every indexing request of `graph_id`, oldest first.
    pub async fn load_index_requests(&self, graph_id: &str) -> anyhow::Result<Vec<IndexRequest>> {
        let rows = sqlx::query(
            "SELECT indexer, blocks, input_tokens, cached_input_tokens, output_tokens, \
             latency_ms, error FROM cg_index_requests WHERE graph_id = ? ORDER BY id",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        let count = |row: &sqlx::sqlite::SqliteRow, column: &str| -> anyhow::Result<u64> {
            Ok(u64::try_from(row.try_get::<i64, _>(column)?).unwrap_or_default())
        };
        rows.iter()
            .map(|row| {
                Ok(IndexRequest {
                    indexer: row.try_get("indexer")?,
                    blocks: u32::try_from(count(row, "blocks")?).unwrap_or(u32::MAX),
                    input_tokens: count(row, "input_tokens")?,
                    cached_input_tokens: count(row, "cached_input_tokens")?,
                    output_tokens: count(row, "output_tokens")?,
                    latency_ms: count(row, "latency_ms")?,
                    error: row.try_get("error")?,
                })
            })
            .collect()
    }

    pub async fn index_usage(&self, graph_id: &str) -> anyhow::Result<IndexUsage> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS requests, COALESCE(SUM(error IS NOT NULL), 0) AS failed, \
             COALESCE(SUM(blocks), 0) AS blocks, COALESCE(SUM(input_tokens), 0) AS input, \
             COALESCE(SUM(cached_input_tokens), 0) AS cached, \
             COALESCE(SUM(output_tokens), 0) AS output, \
             COALESCE(SUM(latency_ms), 0) AS latency, COALESCE(MAX(latency_ms), 0) AS latency_max \
             FROM cg_index_requests WHERE graph_id = ?",
        )
        .bind(graph_id)
        .fetch_one(&self.pool)
        .await?;
        let get = |column: &str| -> anyhow::Result<u64> {
            Ok(u64::try_from(row.try_get::<i64, _>(column)?).unwrap_or_default())
        };
        Ok(IndexUsage {
            requests: get("requests")?,
            failed_requests: get("failed")?,
            blocks: get("blocks")?,
            input_tokens: get("input")?,
            cached_input_tokens: get("cached")?,
            output_tokens: get("output")?,
            latency_ms_total: get("latency")?,
            latency_ms_max: get("latency_max")?,
        })
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| to_i64(elapsed.as_secs()))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "cards_tests.rs"]
mod tests;
