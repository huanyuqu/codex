//! `codex graph`: the context graph of saved conversations.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use codex_context_graph::BlockId;
use codex_context_graph_store::BranchTree;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::SavedThread;
use codex_core::config::ConfigBuilder;
use codex_protocol::ThreadId;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadSortKey;
use codex_thread_store::ThreadStore;
use codex_utils_cli::CliConfigOverrides;

const PAGE_SIZE: usize = 200;

#[derive(Debug, Parser)]
pub(crate) struct GraphCommand {
    #[command(subcommand)]
    action: GraphAction,
}

#[derive(Debug, clap::Subcommand)]
enum GraphAction {
    /// Record saved conversations that ran without the context graph, so their forks show up
    /// as branches in /tree and merges made with the earlier merge tool as merges.
    Import(ImportCommand),
    /// Write the context graph of a conversation: its branches, merges, blocks and cards.
    Export(ExportCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ExportFormat {
    /// The branches as an indented tree with their turns, purposes and merges.
    Tree,
    /// A Mermaid flowchart of branches, forks and merges.
    Mermaid,
    /// The whole graph with cards and merges, as JSON.
    Json,
}

#[derive(Debug, Parser)]
struct ExportCommand {
    /// A conversation of the graph to export, by thread ID.
    #[arg(value_name = "THREAD_ID", value_parser = ThreadId::from_string)]
    thread: ThreadId,

    #[arg(long, value_enum, default_value_t = ExportFormat::Tree)]
    format: ExportFormat,

    /// Write to this file instead of standard output.
    #[arg(long, short = 'o', value_name = "FILE")]
    output: Option<std::path::PathBuf>,
}

#[derive(Debug, Parser)]
struct ImportCommand {
    /// Import only these conversations, by thread ID. Their forks are not included.
    #[arg(value_name = "THREAD_ID", value_parser = ThreadId::from_string)]
    threads: Vec<ThreadId>,

    /// Also import archived conversations.
    #[arg(long)]
    archived: bool,

    /// List what would be imported without recording anything.
    #[arg(long)]
    dry_run: bool,
}

pub(crate) async fn run(
    command: GraphCommand,
    config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let overrides = config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config = ConfigBuilder::default()
        .cli_overrides(overrides)
        .build()
        .await?;
    match command.action {
        GraphAction::Export(export) => {
            let store = LocalContextGraphStore::open(&config.sqlite)
                .await
                .context("failed to open the context graph")?;
            let tree = store
                .branch_tree(&export.thread.to_string())
                .await?
                .with_context(|| {
                    format!(
                        "conversation {} has no context graph; run `codex graph import {}` first",
                        export.thread, export.thread
                    )
                })?;
            let text = match export.format {
                ExportFormat::Tree => tree_text(&tree),
                ExportFormat::Mermaid => {
                    let merges: Vec<_> = tree
                        .merges
                        .iter()
                        .map(|merge| (&merge.source, &merge.target, merge.mode.as_str()))
                        .collect();
                    codex_context_graph::render::branch_mermaid_with_merges(&tree.graph, &merges)
                }
                ExportFormat::Json => serde_json::to_string_pretty(&graph_json(&tree))? + "\n",
            };
            match export.output {
                Some(path) => std::fs::write(&path, text)
                    .with_context(|| format!("failed to write {}", path.display()))?,
                None => print!("{text}"),
            }
            Ok(())
        }
        GraphAction::Import(import) => {
            let state_db = codex_rollout::state_db::try_init(&config)
                .await
                .context("failed to open local thread metadata")?;
            let threads = codex_core::thread_store_from_config(&config, Some(state_db));
            let saved = saved_threads(threads.as_ref(), &import).await?;
            if import.dry_run {
                for thread in &saved {
                    let lineage = thread
                        .forked_from
                        .as_deref()
                        .map(|parent| format!(" (fork of {parent})"))
                        .unwrap_or_default();
                    let name = thread.name.as_deref().unwrap_or("");
                    println!("{}{lineage} {name}", thread.thread_id);
                }
                println!("{} conversations would be imported.", saved.len());
                return Ok(());
            }
            let store = LocalContextGraphStore::open(&config.sqlite)
                .await
                .context("failed to open the context graph")?;
            let threads: Arc<dyn ThreadStore> = threads;
            let report = store.import_threads(threads.as_ref(), &saved).await?;
            println!(
                "Recorded {} conversations; {} were recorded before. Found {} earlier merges.",
                report.recorded, report.already_recorded, report.merges
            );
            for (thread_id, error) in &report.failed {
                eprintln!("Could not import {thread_id}: {error}");
            }
            if !report.failed.is_empty() {
                anyhow::bail!(
                    "{} conversations could not be imported",
                    report.failed.len()
                );
            }
            Ok(())
        }
    }
}

/// The branches as an indented tree, one line each, with their merges.
fn tree_text(tree: &BranchTree) -> String {
    let mut text = String::new();
    for row in tree.rows() {
        let settings = tree.settings_of(&row);
        let mut parts = vec![match row.turns {
            1 => "1 turn".to_string(),
            turns => format!("{turns} turns"),
        }];
        if let Some(purpose) = settings.purpose {
            parts.push(format!("for: {purpose}"));
        }
        if let Some(card) = tree.branch_cards.get(&row.branch) {
            parts.push(card.title.clone());
        }
        if let Some(request) = &row.last_request {
            parts.push(format!("last: {request}"));
        }
        let merged_into = tree.merged_into(&row.branch);
        if !merged_into.is_empty() {
            parts.push(format!("merged into {}", merged_into.join(", ")));
        }
        if row.status != codex_context_graph::BranchStatus::Active {
            parts.push(format!("{:?}", row.status).to_lowercase());
        }
        let thread = row.thread_id.as_deref().unwrap_or("no chat");
        text.push_str(&format!(
            "{}{} ({thread}): {}\n",
            "  ".repeat(row.depth),
            row.name,
            parts.join(" · ")
        ));
    }
    text
}

/// The graph with its cards, merges and branch settings, as one JSON document.
fn graph_json(tree: &BranchTree) -> serde_json::Value {
    let mut cards: Vec<_> = tree.cards.values().collect();
    cards.sort_by(|a, b| a.block_id.as_str().cmp(b.block_id.as_str()));
    let branches: Vec<_> = tree
        .rows()
        .into_iter()
        .map(|row| {
            let settings = tree.settings_of(&row);
            serde_json::json!({
                "branchId": row.branch.as_str(),
                "name": row.name,
                "threadId": row.thread_id,
                "status": format!("{:?}", row.status).to_lowercase(),
                "turns": row.turns,
                "purpose": settings.purpose,
                "summary": tree.branch_cards.get(&row.branch).map(|card| card.title.clone()),
            })
        })
        .collect();
    let merges: Vec<_> = tree
        .merges
        .iter()
        .map(|merge| {
            serde_json::json!({
                "source": merge.source.as_str(),
                "target": merge.target.as_str(),
                "mode": merge.mode.as_str(),
                "sourceHead": merge.source_head.as_ref().map(BlockId::as_str),
                "mergeBlock": merge.merge_block.as_ref().map(BlockId::as_str),
                "goal": merge.goal,
                "code": merge.code,
            })
        })
        .collect();
    serde_json::json!({
        "format": codex_context_graph::GRAPH_FORMAT,
        "graphId": tree.graph_id,
        "branches": branches,
        "merges": merges,
        "graph": tree.graph.export(),
        "cards": cards,
    })
}

/// The conversations to import: the named ones, or every saved one.
async fn saved_threads(
    threads: &dyn ThreadStore,
    command: &ImportCommand,
) -> anyhow::Result<Vec<SavedThread>> {
    let mut stored: Vec<StoredThread> = Vec::new();
    if command.threads.is_empty() {
        let archives = if command.archived {
            vec![false, true]
        } else {
            vec![false]
        };
        for archived in archives {
            let mut cursor = None;
            loop {
                let page = threads
                    .list_threads(ListThreadsParams {
                        page_size: PAGE_SIZE,
                        cursor,
                        sort_key: ThreadSortKey::CreatedAt,
                        sort_direction: SortDirection::Asc,
                        allowed_sources: Vec::new(),
                        model_providers: Some(Vec::new()),
                        cwd_filters: None,
                        section: None,
                        project_id: None,
                        archived,
                        search_term: None,
                        relation_filter: None,
                        use_state_db_only: false,
                    })
                    .await?;
                stored.extend(page.items);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
    } else {
        for thread_id in &command.threads {
            stored.push(
                threads
                    .read_thread(ReadThreadParams {
                        thread_id: *thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await
                    .with_context(|| format!("no saved conversation {thread_id}"))?,
            );
        }
    }
    Ok(stored
        .into_iter()
        .map(|thread| SavedThread {
            thread_id: thread.thread_id.to_string(),
            forked_from: thread.forked_from_id.map(|parent| parent.to_string()),
            name: thread.name,
        })
        .collect())
}
