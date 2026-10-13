//! Indexes recorded rollouts into a context graph database, recalls from them, and reports what
//! indexing and recall cost.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use clap::Parser;
use clap::Subcommand;
use codex_context_graph::card::EXTRACTED_INDEXER;
use codex_context_graph::text::item_text;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::RenderOptions;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::render_view;
use codex_context_graph_eval::provider::Endpoint;
use codex_context_graph_eval::provider::resolve_endpoint;
use codex_context_graph_eval::responses::ResponsesModel;
use codex_context_graph_eval::session::Session;
use codex_context_graph_eval::session::replay;
use codex_context_graph_store::IndexRequest;
use codex_context_graph_store::IndexerConfig;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::ModelCall;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::ShadowIndexer;
use codex_context_graph_store::ShadowModel;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;

#[derive(Debug, Parser)]
#[command(about = "Evaluate context graph indexing and recall on recorded rollouts")]
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
    /// Choose the context for a question asked at some turn of a rollout.
    Recall(RecallArgs),
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

/// Where the models are served and how they are asked.
#[derive(Debug, Clone, clap::Args)]
struct ModelArgs {
    /// Responses API base URL. Defaults to the active provider of the Codex home.
    #[arg(long)]
    base_url: Option<String>,

    #[arg(long, default_value = "OPENAI_API_KEY")]
    api_key_env: String,

    /// Codex home to read the provider and API key from. Defaults to $CODEX_HOME or ~/.codex.
    #[arg(long)]
    codex_home: Option<PathBuf>,

    #[arg(long, default_value_t = 180)]
    timeout_secs: u64,

    #[arg(long, default_value = "low")]
    reasoning_effort: String,
}

impl ModelArgs {
    fn endpoint(&self) -> anyhow::Result<Endpoint> {
        let codex_home = match &self.codex_home {
            Some(home) => home.clone(),
            None => default_codex_home()?,
        };
        resolve_endpoint(&codex_home, self.base_url.clone(), &self.api_key_env)
    }

    fn model(&self, endpoint: &Endpoint, model: &str) -> anyhow::Result<Arc<dyn ShadowModel>> {
        Ok(Arc::new(ResponsesModel::new(
            endpoint.clone(),
            model,
            Some(self.reasoning_effort.clone()),
            Duration::from_secs(self.timeout_secs),
        )?))
    }
}

#[derive(Debug, clap::Args)]
struct IndexArgs {
    /// Rollout files; each becomes its own graph, named by its thread ID.
    #[arg(required = true)]
    rollouts: Vec<PathBuf>,

    /// Indexing model. Without it, blocks only get extracted cards.
    #[arg(long)]
    model: Option<String>,

    #[arg(long, default_value_t = 8)]
    batch_blocks: usize,

    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// Model requests per rollout.
    #[arg(long)]
    max_requests: Option<u64>,

    #[command(flatten)]
    models: ModelArgs,
}

#[derive(Debug, clap::Args)]
struct RecallArgs {
    /// Rollout to recall from; index it first so that its blocks have model cards.
    rollout: PathBuf,

    #[arg(long)]
    question: String,

    /// Recall as of the end of this turn. Defaults to the last turn.
    #[arg(long)]
    turn: Option<usize>,

    /// Screening model.
    #[arg(long)]
    model: String,

    /// Model that reads the blocks screening could not decide. Defaults to the screening model.
    #[arg(long)]
    read_model: Option<String>,

    /// Skip reading; blocks screening could not decide are summarized.
    #[arg(long)]
    no_read: bool,

    #[arg(long, default_value_t = 40_000)]
    budget: u32,

    #[arg(long, default_value_t = 2)]
    recent_turns: usize,

    /// Print the items the model would see.
    #[arg(long)]
    render: bool,

    #[command(flatten)]
    models: ModelArgs,
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
        Command::Recall(recall_args) => recall(&store, recall_args).await,
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
    let model = match &args.model {
        Some(model) => {
            let endpoint = args.models.endpoint()?;
            eprintln!("indexing with {model} at {}", endpoint.base_url);
            Some(args.models.model(&endpoint, model)?)
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
        let replayed = replay(path).await?;
        let graph_id = replayed.graph_id;
        store.replace_graph(&graph_id, &replayed.graph).await?;

        let rollout_started = Instant::now();
        let mut indexer = ShadowIndexer::new(
            store.clone(),
            graph_id.clone(),
            replayed.branch,
            model.clone(),
            config.clone(),
        );
        for event in replayed.events {
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
            replayed.graph.len(),
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

async fn recall(store: &LocalContextGraphStore, args: RecallArgs) -> anyhow::Result<()> {
    let session = Session::load(store, &args.rollout).await?;
    let history = session.history(args.turn);
    println!(
        "{}: {} of {} blocks up to turn {} ({} without a stored card)",
        session.graph_id,
        history.len(),
        session.blocks.len(),
        history.last_turn(),
        session.extracted_cards
    );

    let endpoint = args.models.endpoint()?;
    let screen_model = args.models.model(&endpoint, &args.model)?;
    let read_model = if args.no_read {
        None
    } else {
        let model = args.read_model.as_deref().unwrap_or(&args.model);
        Some(args.models.model(&endpoint, model)?)
    };
    let config = RecallConfig {
        view: ViewLimits {
            budget_tokens: args.budget,
            recent_turns: args.recent_turns,
            ..ViewLimits::default()
        },
        ..RecallConfig::default()
    };
    let recall = Recaller::new(screen_model, read_model, config)
        .recall(&history, &args.question)
        .await?;
    store
        .record_view(
            &session.graph_id,
            &session.branch,
            &args.question,
            &args.model,
            &recall.view,
            &recall.stats,
        )
        .await?;

    let stats = &recall.stats;
    let view = &recall.view;
    println!(
        "screening: {} candidates (~{} tokens{}), {} picked, {} uncertain, {} read; {:.1}s",
        stats.candidates,
        stats.catalog_tokens,
        if stats.prefiltered {
            ", prefiltered"
        } else {
            ""
        },
        stats.picked,
        stats.uncertain,
        stats.read,
        stats.latency_ms as f64 / 1000.0
    );
    println!(
        "view: {} blocks, ~{} of {} tokens ({} shrunk, {} dropped), {} ledger claims, {} map lines",
        view.entries.len(),
        view.tokens,
        view.budget_tokens,
        view.shrunk,
        view.dropped,
        view.ledger.len(),
        view.map.len()
    );
    for entry in &view.entries {
        let detail = match &entry.presentation {
            Presentation::Full => "full".to_string(),
            Presentation::Excerpt { lines } => format!("excerpt x{}", lines.len()),
            Presentation::Summary => "summary".to_string(),
        };
        let reason = format!("{:?}", entry.reason).to_lowercase();
        let mut line = format!(
            "  turn {:>3}  {detail:<10} {reason:<10} {:>6}t  {}",
            history.turn(entry.index),
            entry.tokens,
            history.card(entry.index).title
        );
        if let Some(note) = &entry.note {
            line.push_str(&format!("  ({note})"));
        }
        println!("{line}");
    }
    if let Some(call) = &stats.screen {
        print_call("screen", call);
    }
    for call in &stats.reads {
        print_call("read", call);
    }
    if args.render {
        println!("\n----- rendered view -----");
        for item in render_view(&history, view, RenderOptions::default()) {
            if let Some(text) = item_text(&item) {
                println!("[{}]\n{}\n", text.label, text.text);
            }
        }
        println!("[user]\n{}", args.question);
    }
    Ok(())
}

fn print_call(name: &str, call: &ModelCall) {
    let error = call
        .error
        .as_deref()
        .map(|error| format!(", error: {error}"))
        .unwrap_or_default();
    println!(
        "{name} {}: {} input ({} cached), {} output, {:.1}s{error}",
        call.model,
        call.input_tokens,
        call.cached_input_tokens,
        call.output_tokens,
        call.latency_ms as f64 / 1000.0,
    );
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
