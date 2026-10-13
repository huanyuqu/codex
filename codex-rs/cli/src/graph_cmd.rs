//! `codex graph`: the context graph of saved conversations.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
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
