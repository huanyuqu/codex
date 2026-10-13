//! Indexes recorded rollouts into a context graph database and reports indexing cost.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use clap::Parser;
use clap::Subcommand;
use codex_context_graph::Branch;
use codex_context_graph::BranchId;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphRecorder;
use codex_context_graph::card::EXTRACTED_INDEXER;
use codex_context_graph_eval::provider::resolve_endpoint;
use codex_context_graph_eval::responses::ResponsesModel;
use codex_context_graph_store::CardModel;
use codex_context_graph_store::IndexRequest;
use codex_context_graph_store::IndexerConfig;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::ShadowIndexer;
use codex_rollout::RolloutRecorder;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Parser)]
#[command(about = "Evaluate context graph indexing on recorded rollouts")]
struct Args {
    /// Directory of the context graph database to write and read.
    #[arg(long, global = true, default_value = ".")]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Replay rollouts into graphs and index their blocks.
    Index(IndexArgs),
    /// Print the stored cards of a graph as JSON lines.
    Cards {
        #[arg(long)]
        graph: String,
    },
    /// Print indexing cost and latency of one graph, or of every graph.
    Usage {
        #[arg(long)]
        graph: Option<String>,
    },
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Rollout files; each becomes its own graph, named by its thread ID.
    #[arg(required = true)]
    rollouts: Vec<PathBuf>,

    /// Indexing model. Without it, blocks only get extracted cards.
    #[arg(long)]
    model: Option<String>,

    #[arg(long, default_value = "low")]
    reasoning_effort: String,

    #[arg(long, default_value_t = 8)]
    batch_blocks: usize,

    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// Model requests per rollout.
    #[arg(long)]
    max_requests: Option<u64>,

    /// Responses API base URL. Defaults to the active provider of the Codex home.
    #[arg(long)]
    base_url: Option<String>,

    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    /// Codex home to read the provider and API key from. Defaults to $CODEX_HOME or ~/.codex.
    #[arg(long)]
    codex_home: Option<PathBuf>,

    #[arg(long, default_value_t = 120)]
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    let args = Args::parse();
    let db = std::path::absolute(&args.db)?;
    let db = AbsolutePathBuf::from_absolute_path(&db)
        .with_context(|| format!("{} is not an absolute path", db.display()))?;
    let store = LocalContextGraphStore::open(&SqliteConfig::from_sqlite_home(db)).await?;
    match args.command {
        Command::Index(index_args) => index(&store, index_args).await,
        Command::Cards { graph } => {
            for card in store.load_cards(&graph).await? {
                println!("{}", serde_json::to_string(&card)?);
            }
            Ok(())
        }
        Command::Usage { graph } => {
            let graphs = match graph {
                Some(graph) => vec![graph],
                None => store.list_graph_ids().await?,
            };
            let mut requests = Vec::new();
            for graph in &graphs {
                requests.extend(store.load_index_requests(graph).await?);
            }
            print_usage(&requests, None);
            Ok(())
        }
    }
}

async fn index(store: &LocalContextGraphStore, args: IndexArgs) -> anyhow::Result<()> {
    let model: Option<Arc<dyn CardModel>> = match &args.model {
        Some(model) => {
            let codex_home = match &args.codex_home {
                Some(home) => home.clone(),
                None => default_codex_home()?,
            };
            let endpoint = resolve_endpoint(&codex_home, args.base_url.clone(), &args.api_key_env)?;
            eprintln!("indexing with {model} at {}", endpoint.base_url);
            Some(Arc::new(ResponsesModel::new(
                endpoint,
                model.clone(),
                Some(args.reasoning_effort.clone()),
                Duration::from_secs(args.timeout_secs),
            )?))
        }
        None => None,
    };
    let config = IndexerConfig {
        batch_blocks: args.batch_blocks,
        max_concurrent_requests: args.concurrency,
        max_requests: args.max_requests,
        ..IndexerConfig::default()
    };

    let started = Instant::now();
    let mut all_requests = Vec::new();
    let mut total_cards = 0usize;
    let mut total_model_cards = 0usize;
    for path in &args.rollouts {
        let (items, thread_id, _) = RolloutRecorder::load_rollout_items(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let graph_id = thread_id.map_or_else(
            || {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            |thread_id| thread_id.to_string(),
        );
        let branch_id = BranchId::for_thread(&graph_id);
        let mut graph = ContextGraph::new();
        let mut branch = Branch::new(branch_id.clone(), graph_id.clone());
        branch.thread_id = Some(graph_id.clone());
        graph.add_branch(branch)?;
        let mut recorder = GraphRecorder::new(branch_id.clone(), DefaultClassifier).with_events();
        let mut events = Vec::new();
        for item in &items {
            recorder.observe(&mut graph, item)?;
            events.extend(recorder.take_events());
        }
        recorder.finish(&mut graph)?;
        events.extend(recorder.take_events());
        store.replace_graph(&graph_id, &graph).await?;

        let rollout_started = Instant::now();
        let mut indexer = ShadowIndexer::new(
            store.clone(),
            graph_id.clone(),
            branch_id,
            model.clone(),
            config.clone(),
        );
        for event in events {
            indexer.observe(event).await?;
        }
        indexer.finish().await;

        let cards = store.load_cards(&graph_id).await?;
        let model_cards = cards
            .iter()
            .filter(|card| card.indexer != EXTRACTED_INDEXER)
            .count();
        let requests = store.load_index_requests(&graph_id).await?;
        eprintln!(
            "{graph_id}: {} blocks, {} cards ({model_cards} by model), {} requests, {:.1}s",
            graph.len(),
            cards.len(),
            requests.len(),
            rollout_started.elapsed().as_secs_f64()
        );
        total_cards += cards.len();
        total_model_cards += model_cards;
        all_requests.extend(requests);
    }
    println!(
        "{} rollouts, {total_cards} cards ({total_model_cards} by model) in {:.1}s",
        args.rollouts.len(),
        started.elapsed().as_secs_f64()
    );
    print_usage(&all_requests, Some(total_model_cards));
    Ok(())
}

fn print_usage(requests: &[IndexRequest], model_cards: Option<usize>) {
    if requests.is_empty() {
        println!("no model requests");
        return;
    }
    let failed = requests
        .iter()
        .filter(|request| request.error.is_some())
        .count();
    let sum = |field: fn(&IndexRequest) -> u64| requests.iter().map(field).sum::<u64>();
    let input = sum(|request| request.input_tokens);
    let cached = sum(|request| request.cached_input_tokens);
    let output = sum(|request| request.output_tokens);
    let blocks = sum(|request| u64::from(request.blocks));
    let mut latencies: Vec<u64> = requests.iter().map(|request| request.latency_ms).collect();
    latencies.sort_unstable();
    let percentile = |p: usize| latencies[(latencies.len() - 1) * p / 100];
    println!(
        "{} requests ({failed} failed) for {blocks} blocks",
        requests.len()
    );
    println!(
        "tokens: {input} input ({cached} cached), {output} output; per block {:.0} input, {:.0} output",
        input as f64 / blocks.max(1) as f64,
        output as f64 / blocks.max(1) as f64
    );
    println!(
        "latency per request: p50 {} ms, p95 {} ms, max {} ms",
        percentile(50),
        percentile(95),
        percentile(100)
    );
    if let Some(model_cards) = model_cards {
        println!("model cards: {model_cards}");
    }
    for request in requests
        .iter()
        .filter(|request| request.error.is_some())
        .take(3)
    {
        println!("error: {}", request.error.as_deref().unwrap_or_default());
    }
}

fn default_codex_home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --codex-home")?;
    Ok(PathBuf::from(home).join(".codex"))
}
