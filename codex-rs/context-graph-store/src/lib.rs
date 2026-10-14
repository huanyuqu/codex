//! SQLite index of context graphs.
//!
//! The index is derived data: rollouts stay the source of truth, and every stored graph can be
//! rebuilt by replaying them. It lives in its own database file with its own migrations, so it
//! never competes with the state database's migration numbering.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;

use anyhow::Context as _;
use codex_context_graph::Block;
use codex_context_graph::BlockId;
use codex_context_graph::BlockNode;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::GRAPH_FORMAT;
use codex_context_graph::GraphExport;
use codex_state::SqliteConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::Row;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use tokio::sync::OnceCell;

mod branch_tree;
mod cards;
mod consolidate;
mod feedback;
mod forget;
mod import;
mod indexer;
mod live;
mod merges;
mod model;
mod recall;
mod scorer;
mod settings;
mod snapshot;
mod views;

pub use branch_tree::BranchTree;
pub use branch_tree::MergeError;
pub use branch_tree::MergePlan;
pub use cards::IndexRequest;
pub use cards::IndexUsage;
pub use consolidate::Consolidation;
pub use feedback::RecallFeedback;
pub use feedback::RecallFeedbackCounts;
pub use import::ImportReport;
pub use import::SavedItems;
pub use import::SavedThread;
pub use indexer::CardsListener;
pub use indexer::IndexerConfig;
pub use indexer::ShadowIndexer;
pub use live::GraphMembership;
pub use live::LiveContextGraph;
pub use live::LiveIndexing;
pub use live::install_live_recorder;
pub use live::saved_items;
pub use merges::CodeMerge;
pub use merges::MergeMode;
pub use merges::MergeRecord;
pub use model::ModelCompletion;
pub use model::ShadowModel;
pub use recall::ModelCall;
pub use recall::Recall;
pub use recall::RecallConfig;
pub use recall::RecallDelivery;
pub use recall::RecallStats;
pub use recall::Recaller;
pub use recall::ScreenStrategy;
pub use recall::SelectedBlock;
pub use recall::Selection;
pub use scorer::ModelScorer;
pub use scorer::RelevanceScorer;
pub use scorer::RerankScorer;
pub use settings::BranchSettings;
pub use snapshot::BranchSnapshot;
pub use snapshot::replay_blocks;
pub use views::ViewRecord;

pub const CONTEXT_GRAPH_DB_FILENAME: &str = "context_graph_1.sqlite";

static MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations");

/// Context graph index stored next to Codex's other SQLite databases.
#[derive(Debug, Clone)]
pub struct LocalContextGraphStore {
    pool: SqlitePool,
}

/// Stores opened by this process, by database path. Sessions share one pool per database, and
/// a new database is migrated once rather than by every session that opens it concurrently.
static OPENED: LazyLock<Mutex<HashMap<PathBuf, Arc<OnceCell<LocalContextGraphStore>>>>> =
    LazyLock::new(Mutex::default);

impl LocalContextGraphStore {
    pub async fn open(sqlite: &SqliteConfig) -> anyhow::Result<Self> {
        let path = sqlite.home().join(CONTEXT_GRAPH_DB_FILENAME);
        let cell = match OPENED.lock() {
            Ok(mut opened) => Arc::clone(opened.entry(path.to_path_buf()).or_default()),
            Err(_) => Arc::new(OnceCell::new()),
        };
        let store = cell
            .get_or_try_init(|| Self::open_and_migrate(sqlite, &path))
            .await?;
        Ok(store.clone())
    }

    async fn open_and_migrate(sqlite: &SqliteConfig, path: &Path) -> anyhow::Result<Self> {
        let pool = sqlite.open_read_write_pool(path).await?;
        // Tolerate a database that a newer binary already migrated further.
        let migrator = Migrator {
            migrations: Cow::Borrowed(MIGRATOR.migrations.as_ref()),
            ignore_missing: true,
            locking: MIGRATOR.locking,
            no_tx: MIGRATOR.no_tx,
            table_name: MIGRATOR.table_name.clone(),
            create_schemas: MIGRATOR.create_schemas.clone(),
        };
        migrator
            .run(&pool)
            .await
            .context("failed to migrate the context graph database")?;
        Ok(Self { pool })
    }

    /// Replaces everything stored for `graph_id` with `graph`.
    pub async fn replace_graph(&self, graph_id: &str, graph: &ContextGraph) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM cg_edges WHERE graph_id = ?")
            .bind(graph_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM cg_blocks WHERE graph_id = ?")
            .bind(graph_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM cg_branches WHERE graph_id = ?")
            .bind(graph_id)
            .execute(&mut *tx)
            .await?;
        for (seq, node) in graph.nodes().enumerate() {
            insert_block(&mut tx, graph_id, to_i64(seq), node).await?;
        }
        for branch in graph.branches() {
            upsert_branch(&mut tx, graph_id, branch).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Adds the blocks and branches of `graph` that are not stored yet and updates its branches,
    /// keeping everything else stored for `graph_id`, such as the other branches of a shared
    /// graph.
    pub async fn merge_graph(&self, graph_id: &str, graph: &ContextGraph) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut next_seq = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM cg_blocks WHERE graph_id = ?",
        )
        .bind(graph_id)
        .fetch_one(&mut *tx)
        .await?;
        for node in graph.nodes() {
            if insert_block(&mut tx, graph_id, next_seq, node).await? {
                next_seq += 1;
            }
        }
        for branch in graph.branches() {
            upsert_branch(&mut tx, graph_id, branch).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The graph and branch that `thread_id` records into, when it has been recorded.
    pub async fn branch_of_thread(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<(String, Branch)>> {
        // A thread's rolled-back history is stored as more branches of the same thread.
        let Some(row) = sqlx::query(
            "SELECT graph_id, branch_id FROM cg_branches WHERE thread_id = ? \
             ORDER BY status = 'active' DESC, branch_id LIMIT 1",
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let graph_id: String = row.try_get("graph_id")?;
        let branch_id = BranchId::new(row.try_get::<String, _>("branch_id")?);
        let branch = self
            .load_graph(&graph_id)
            .await?
            .and_then(|graph| graph.branch(&branch_id).cloned());
        Ok(branch.map(|branch| (graph_id, branch)))
    }

    /// Stores a block that was just recorded, after every block already stored for `graph_id`.
    ///
    /// Blocks that are already stored, such as a prefix shared with another branch, are kept.
    pub async fn append_block(&self, graph_id: &str, node: &BlockNode) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let next_seq = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(seq) + 1, 0) FROM cg_blocks WHERE graph_id = ?",
        )
        .bind(graph_id)
        .fetch_one(&mut *tx)
        .await?;
        insert_block(&mut tx, graph_id, next_seq, node).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Stores a new branch or a branch whose head or status changed.
    pub async fn upsert_branch(&self, graph_id: &str, branch: &Branch) -> anyhow::Result<()> {
        let mut connection = self.pool.acquire().await?;
        upsert_branch(&mut connection, graph_id, branch).await
    }

    /// Renames a branch. Recording keeps the stored name, so the new name sticks.
    pub async fn rename_branch(
        &self,
        graph_id: &str,
        branch: &BranchId,
        name: &str,
    ) -> anyhow::Result<bool> {
        let renamed =
            sqlx::query("UPDATE cg_branches SET name = ? WHERE graph_id = ? AND branch_id = ?")
                .bind(name)
                .bind(graph_id)
                .bind(branch.as_str())
                .execute(&self.pool)
                .await?
                .rows_affected();
        Ok(renamed > 0)
    }

    /// Names the branch `thread_id` records into, now if it has one and otherwise once it
    /// first records. Returns the graph of the renamed branch, if there was one.
    pub async fn name_thread_branch(
        &self,
        thread_id: &str,
        name: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO cg_thread_names (thread_id, name) VALUES (?, ?) \
             ON CONFLICT(thread_id) DO UPDATE SET name = excluded.name",
        )
        .bind(thread_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        let graph_id = sqlx::query_scalar::<_, String>(
            "UPDATE cg_branches SET name = ? WHERE thread_id = ? RETURNING graph_id",
        )
        .bind(name)
        .bind(thread_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(graph_id)
    }

    /// The name given to `thread_id`'s branch before it recorded, if any.
    pub async fn thread_branch_name(&self, thread_id: &str) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar::<_, String>("SELECT name FROM cg_thread_names WHERE thread_id = ?")
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Loads and validates a stored graph.
    pub async fn load_graph(&self, graph_id: &str) -> anyhow::Result<Option<ContextGraph>> {
        let branch_rows = sqlx::query(
            "SELECT branch_id, name, thread_id, parent_branch_id, fork_point, head, status \
             FROM cg_branches WHERE graph_id = ? ORDER BY branch_id",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        let block_rows = sqlx::query(
            "SELECT block_id, kind, owner_branch_id, turn_id, item_keys, first_ordinal, \
             last_ordinal, approx_bytes, complete FROM cg_blocks WHERE graph_id = ? ORDER BY seq",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;
        if branch_rows.is_empty() && block_rows.is_empty() {
            return Ok(None);
        }
        let edge_rows = sqlx::query(
            "SELECT child_block_id, parent_block_id FROM cg_edges \
             WHERE graph_id = ? ORDER BY child_block_id, position",
        )
        .bind(graph_id)
        .fetch_all(&self.pool)
        .await?;

        let mut parents: HashMap<String, Vec<BlockId>> = HashMap::new();
        for row in edge_rows {
            let child: String = row.try_get("child_block_id")?;
            let parent: String = row.try_get("parent_block_id")?;
            parents
                .entry(child)
                .or_default()
                .push(BlockId::from_stored(parent));
        }

        let mut branches = Vec::with_capacity(branch_rows.len());
        for row in branch_rows {
            branches.push(Branch {
                id: BranchId::new(row.try_get::<String, _>("branch_id")?),
                name: row.try_get("name")?,
                thread_id: row.try_get("thread_id")?,
                parent: row
                    .try_get::<Option<String>, _>("parent_branch_id")?
                    .map(BranchId::new),
                fork_point: row
                    .try_get::<Option<String>, _>("fork_point")?
                    .map(BlockId::from_stored),
                head: row
                    .try_get::<Option<String>, _>("head")?
                    .map(BlockId::from_stored),
                status: from_text(row.try_get("status")?)?,
            });
        }

        let mut blocks = Vec::with_capacity(block_rows.len());
        for row in block_rows {
            let id: String = row.try_get("block_id")?;
            let item_keys: String = row.try_get("item_keys")?;
            blocks.push(BlockNode {
                parents: parents.remove(&id).unwrap_or_default(),
                block: Block {
                    id: BlockId::from_stored(id),
                    kind: from_text(row.try_get("kind")?)?,
                    item_keys: serde_json::from_str(&item_keys)?,
                    turn_id: row.try_get("turn_id")?,
                    first_ordinal: to_u64(row.try_get("first_ordinal")?),
                    last_ordinal: to_u64(row.try_get("last_ordinal")?),
                    approx_bytes: to_u64(row.try_get("approx_bytes")?),
                    complete: row.try_get("complete")?,
                },
                owner: BranchId::new(row.try_get::<String, _>("owner_branch_id")?),
            });
        }

        let graph = ContextGraph::import(GraphExport {
            format: GRAPH_FORMAT.to_string(),
            branches,
            blocks,
        })
        .with_context(|| format!("stored context graph {graph_id} is invalid"))?;
        Ok(Some(graph))
    }

    pub async fn list_graph_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT graph_id FROM cg_branches ORDER BY graph_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }
}

/// Stores `node` unless it is stored already; returns whether it was new.
async fn insert_block(
    connection: &mut SqliteConnection,
    graph_id: &str,
    seq: i64,
    node: &BlockNode,
) -> anyhow::Result<bool> {
    let block = &node.block;
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO cg_blocks (graph_id, block_id, seq, kind, owner_branch_id, turn_id, \
         item_keys, first_ordinal, last_ordinal, approx_bytes, complete) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(graph_id)
    .bind(block.id.as_str())
    .bind(seq)
    .bind(to_text(&block.kind)?)
    .bind(node.owner.as_str())
    .bind(block.turn_id.as_deref())
    .bind(serde_json::to_string(&block.item_keys)?)
    .bind(to_i64(block.first_ordinal))
    .bind(to_i64(block.last_ordinal))
    .bind(to_i64(block.approx_bytes))
    .bind(block.complete)
    .execute(&mut *connection)
    .await?
    .rows_affected()
        > 0;
    for (position, parent) in node.parents.iter().enumerate() {
        sqlx::query(
            "INSERT OR IGNORE INTO cg_edges (graph_id, child_block_id, position, parent_block_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(graph_id)
        .bind(block.id.as_str())
        .bind(to_i64(position))
        .bind(parent.as_str())
        .execute(&mut *connection)
        .await?;
    }
    Ok(inserted)
}

async fn upsert_branch(
    connection: &mut SqliteConnection,
    graph_id: &str,
    branch: &Branch,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO cg_branches (graph_id, branch_id, name, thread_id, parent_branch_id, \
         fork_point, head, status) VALUES (?, ?, \
         COALESCE((SELECT name FROM cg_thread_names WHERE thread_id = ?), ?), ?, ?, ?, ?, ?) \
         ON CONFLICT(graph_id, branch_id) DO UPDATE SET thread_id = excluded.thread_id, \
         parent_branch_id = excluded.parent_branch_id, fork_point = excluded.fork_point, \
         head = excluded.head, status = excluded.status",
    )
    .bind(graph_id)
    .bind(branch.id.as_str())
    .bind(branch.thread_id.as_deref())
    .bind(&branch.name)
    .bind(branch.thread_id.as_deref())
    .bind(branch.parent.as_ref().map(BranchId::as_str))
    .bind(branch.fork_point.as_ref().map(BlockId::as_str))
    .bind(branch.head.as_ref().map(BlockId::as_str))
    .bind(to_text(&branch.status)?)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// Stores a unit enum by its serde name.
fn to_text<T: Serialize>(value: &T) -> anyhow::Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(text) => Ok(text),
        other => anyhow::bail!("expected a string-valued enum, got {other}"),
    }
}

fn from_text<T: DeserializeOwned>(text: String) -> anyhow::Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(text))?)
}

fn to_i64(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
