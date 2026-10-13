//! Replays Codex rollouts into a context graph, validates it, and optionally stores it.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use clap::ValueEnum;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::IndexUpdate;
use codex_context_graph::ReplayStats;
use codex_context_graph::index_rollout;
use codex_context_graph::render;
use codex_context_graph_store::LocalContextGraphStore;
use codex_rollout::RolloutRecorder;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Parser)]
#[command(about = "Replay Codex rollouts into a context graph")]
struct Args {
    /// Rollout files to replay. List a source thread before the threads forked from it.
    #[arg(required = true)]
    rollouts: Vec<PathBuf>,

    #[arg(long, value_enum, default_value_t = Format::Summary)]
    format: Format,

    /// Store the replayed graph under this ID, then read it back and verify it.
    #[arg(long, value_name = "GRAPH_ID")]
    save: Option<String>,

    /// Directory with Codex's SQLite databases. Defaults to $CODEX_HOME or ~/.codex.
    #[arg(long)]
    sqlite_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Summary,
    Json,
    Mermaid,
    /// Extracted block and turn cards as JSON lines.
    Cards,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut graph = ContextGraph::new();
    let mut replays = Vec::new();
    for path in &args.rollouts {
        let (items, thread_id, parse_errors) = RolloutRecorder::load_rollout_items(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let thread = thread_id.map_or_else(
            || {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            |thread_id| thread_id.to_string(),
        );
        let branch_id = BranchId::for_thread(&thread);
        let mut branch = Branch::new(branch_id.clone(), thread.clone());
        branch.thread_id = Some(thread.clone());
        graph.add_branch(branch)?;
        let print_cards = args.format == Format::Cards;
        let stats = index_rollout(
            &mut graph,
            &branch_id,
            &items,
            DefaultClassifier,
            |update| {
                if print_cards {
                    print_card(&update);
                }
            },
        )?;
        replays.push((thread, stats, parse_errors));
    }
    graph.validate()?;

    match args.format {
        Format::Summary => print_summary(&graph, &replays),
        Format::Json => println!("{}", serde_json::to_string_pretty(&graph.export())?),
        Format::Mermaid => print!("{}", render::branch_mermaid(&graph)),
        Format::Cards => {}
    }

    if let Some(graph_id) = args.save {
        let home = match args.sqlite_home {
            Some(home) => home,
            None => default_codex_home()?,
        };
        let home = AbsolutePathBuf::from_absolute_path(&home)
            .with_context(|| format!("{} is not an absolute path", home.display()))?;
        let store = LocalContextGraphStore::open(&SqliteConfig::from_sqlite_home(home)).await?;
        store.replace_graph(&graph_id, &graph).await?;
        let stored = store
            .load_graph(&graph_id)
            .await?
            .context("stored graph could not be read back")?;
        anyhow::ensure!(
            stored.export() == graph.export(),
            "stored graph differs from the replayed graph"
        );
        eprintln!("stored and verified graph {graph_id}");
    }
    Ok(())
}

fn print_card(update: &IndexUpdate) {
    let line = match update {
        IndexUpdate::Block { card, .. } => serde_json::to_string(&serde_json::json!({
            "type": "block",
            "card": card,
        })),
        IndexUpdate::TurnEnded { card: Some(card) } => serde_json::to_string(&serde_json::json!({
            "type": "turn",
            "card": card,
        })),
        IndexUpdate::TurnEnded { card: None } => return,
    };
    if let Ok(line) = line {
        println!("{line}");
    }
}

fn print_summary(graph: &ContextGraph, replays: &[(String, ReplayStats, usize)]) {
    println!(
        "{} blocks, {} branches",
        graph.len(),
        graph.branches().count()
    );
    for (kind, count) in render::kind_counts(graph) {
        println!("  {kind}: {count}");
    }
    println!();
    let owned = render::owned_block_counts(graph);
    for branch in graph.branches() {
        let parent = branch
            .parent
            .as_ref()
            .map_or_else(String::new, |parent| format!(" from {parent}"));
        let fork_point = branch
            .fork_point
            .as_ref()
            .map_or_else(String::new, |block| format!(" after {block}"));
        println!(
            "{} [{:?}] {} owned blocks{parent}{fork_point}",
            branch.name,
            branch.status,
            owned.get(&branch.id).copied().unwrap_or_default(),
        );
    }
    println!();
    for (thread, stats, parse_errors) in replays {
        println!(
            "{thread}: {} rollout items, {} added, {} inherited, {} reincluded, {} rollbacks, {parse_errors} unparsed lines",
            stats.rollout_items,
            stats.blocks_added,
            stats.blocks_inherited,
            stats.blocks_reincluded,
            stats.rollbacks,
        );
    }
}

fn default_codex_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --sqlite-home")?;
    Ok(PathBuf::from(home).join(".codex"))
}
