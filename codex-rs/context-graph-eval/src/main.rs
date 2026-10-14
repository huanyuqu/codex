//! Indexes recorded rollouts into a context graph database, recalls from them, and reports what
//! indexing and recall cost.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
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
use codex_context_graph_eval::bench::Condition;
use codex_context_graph_eval::bench::ContextBuilder;
use codex_context_graph_eval::bench::ProbeResult;
use codex_context_graph_eval::bench::answer_and_judge;
use codex_context_graph_eval::probe::Probe;
use codex_context_graph_eval::probe::append_jsonl;
use codex_context_graph_eval::probe::build_generation_prompt;
use codex_context_graph_eval::probe::parse_generated_probes;
use codex_context_graph_eval::probe::read_jsonl;
use codex_context_graph_eval::provider::Endpoint;
use codex_context_graph_eval::provider::resolve_endpoint;
use codex_context_graph_eval::responses::ResponsesModel;
use codex_context_graph_eval::session::load_session;
use codex_context_graph_eval::session::replay;
use codex_context_graph_store::BranchSnapshot;
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
use futures::StreamExt as _;

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
    /// Write probe questions about rollouts with a strong model.
    ProbeGen(ProbeGenArgs),
    /// Ask probes under several contexts and grade the answers.
    ProbeRun(ProbeRunArgs),
    /// Summarize graded probe answers by condition and category.
    ProbeReport(ProbeReportArgs),
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

    /// Share of the budget, in percent, for summaries of lexical matches the screening did not
    /// pick; 0 adds none.
    #[arg(long, default_value_t = 15)]
    lexical_share: u8,

    /// Print the items the model would see.
    #[arg(long)]
    render: bool,

    #[command(flatten)]
    models: ModelArgs,
}

/// How recall screens and budgets a view of `budget` tokens.
fn recall_config(budget: u32, recent_turns: usize, lexical_share: u8) -> RecallConfig {
    let lexical_share = lexical_share.min(50);
    RecallConfig {
        view: ViewLimits {
            budget_tokens: budget,
            recent_turns,
            lexical_budget_tokens: budget.saturating_mul(u32::from(lexical_share)) / 100,
            ..ViewLimits::default()
        },
        max_matches: if lexical_share == 0 {
            0
        } else {
            RecallConfig::default().max_matches
        },
        ..RecallConfig::default()
    }
}

#[derive(Debug, clap::Args)]
struct ProbeGenArgs {
    /// Rollouts to write probes about; index them first so that recall can use model cards.
    #[arg(required = true)]
    rollouts: Vec<PathBuf>,

    /// Generating model.
    #[arg(long)]
    model: String,

    /// Probes per rollout.
    #[arg(long, default_value_t = 6)]
    count: usize,

    /// Ask the probes after this turn. Defaults to the last turn.
    #[arg(long)]
    turn: Option<usize>,

    /// JSON-lines file the probes are appended to.
    #[arg(long)]
    out: PathBuf,

    #[command(flatten)]
    models: ModelArgs,
}

#[derive(Debug, clap::Args)]
struct ProbeRunArgs {
    #[arg(long)]
    probes: PathBuf,

    /// Comma-separated conditions: full, window, bm25, oracle, recall:SCREEN[+READ|+none].
    #[arg(long, value_delimiter = ',')]
    conditions: Vec<String>,

    #[arg(long)]
    answer_model: String,

    #[arg(long)]
    judge_model: String,

    /// JSON-lines file results are appended to; probes and conditions it has are skipped.
    #[arg(long)]
    out: PathBuf,

    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    #[arg(long, default_value_t = 40_000)]
    budget: u32,

    #[arg(long, default_value_t = 2)]
    recent_turns: usize,

    /// Share of the budget, in percent, for summaries of lexical matches the screening did not
    /// pick; 0 adds none.
    #[arg(long, default_value_t = 15)]
    lexical_share: u8,

    #[arg(long, default_value_t = 200_000)]
    max_full_tokens: u32,

    #[command(flatten)]
    models: ModelArgs,
}

#[derive(Debug, clap::Args)]
struct ProbeReportArgs {
    #[arg(long)]
    results: PathBuf,

    /// Count only probes that the oracle context answers correctly.
    #[arg(long)]
    valid_only: bool,
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
        Command::ProbeGen(probe_args) => probe_gen(&store, probe_args).await,
        Command::ProbeRun(run_args) => probe_run(&store, run_args).await,
        Command::ProbeReport(report_args) => probe_report(&report_args),
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
    let session = load_session(store, &args.rollout).await?;
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
    let config = recall_config(args.budget, args.recent_turns, args.lexical_share);
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

async fn probe_gen(store: &LocalContextGraphStore, args: ProbeGenArgs) -> anyhow::Result<()> {
    let endpoint = args.models.endpoint()?;
    let model = args.models.model(&endpoint, &args.model)?;
    let existing: Vec<Probe> = read_jsonl(&args.out)?;
    for path in &args.rollouts {
        let session = load_session(store, path).await?;
        let history = session.history(args.turn);
        let turn = history.last_turn();
        let prompt = build_generation_prompt(&history, args.count);
        let started = Instant::now();
        let completion = match model.complete(&prompt.prompt).await {
            Ok(completion) => completion,
            Err(err) => {
                eprintln!("{}: generation failed: {err:#}", session.graph_id);
                continue;
            }
        };
        let generated = match parse_generated_probes(&prompt, &history, &completion.text) {
            Ok(generated) => generated,
            Err(err) => {
                eprintln!("{}: {err}", session.graph_id);
                continue;
            }
        };
        let prefix = format!("{}#{turn}#", session.graph_id);
        let first = existing
            .iter()
            .filter(|probe| probe.id.starts_with(&prefix))
            .count();
        for (offset, fields) in generated.into_iter().enumerate() {
            let probe = Probe {
                id: format!("{prefix}{}", first + offset),
                rollout: path.clone(),
                graph_id: session.graph_id.clone(),
                turn,
                category: fields.category,
                question: fields.question,
                answer: fields.answer,
                key_facts: fields.key_facts,
                support: fields.support,
                outdated: fields.outdated,
                generator: args.model.clone(),
            };
            append_jsonl(&args.out, &probe)?;
            println!("{} [{}] {}", probe.id, probe.category, probe.question);
        }
        eprintln!(
            "{}: {} input, {} output tokens, {:.1}s",
            session.graph_id,
            completion.input_tokens,
            completion.output_tokens,
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

async fn probe_run(store: &LocalContextGraphStore, args: ProbeRunArgs) -> anyhow::Result<()> {
    let probes: Vec<Probe> = read_jsonl(&args.probes)?;
    let done: HashSet<(String, String)> = read_jsonl::<ProbeResult>(&args.out)?
        .into_iter()
        .map(|result| (result.probe_id, result.condition))
        .collect();
    let conditions = args
        .conditions
        .iter()
        .map(|name| name.parse::<Condition>())
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut sessions: HashMap<PathBuf, BranchSnapshot> = HashMap::new();
    for probe in &probes {
        if !sessions.contains_key(&probe.rollout) {
            let session = load_session(store, &probe.rollout).await?;
            sessions.insert(probe.rollout.clone(), session);
        }
    }

    let endpoint = args.models.endpoint()?;
    let answer_model = args.models.model(&endpoint, &args.answer_model)?;
    let judge_model = args.models.model(&endpoint, &args.judge_model)?;
    let config = recall_config(args.budget, args.recent_turns, args.lexical_share);
    let mut recallers: HashMap<String, Recaller> = HashMap::new();
    for condition in &conditions {
        if let Condition::Recall { screen, read } = condition {
            let read = match read {
                Some(read) => Some(args.models.model(&endpoint, read)?),
                None => None,
            };
            recallers.insert(
                condition.to_string(),
                Recaller::new(args.models.model(&endpoint, screen)?, read, config),
            );
        }
    }
    let builder = ContextBuilder {
        config,
        max_full_tokens: args.max_full_tokens,
    };

    let tasks: Vec<(&Probe, &Condition)> = probes
        .iter()
        .flat_map(|probe| conditions.iter().map(move |condition| (probe, condition)))
        .filter(|(probe, condition)| !done.contains(&(probe.id.clone(), condition.to_string())))
        .collect();
    let total = tasks.len();
    eprintln!("{total} answers to run");
    let mut results = futures::stream::iter(tasks.into_iter().map(|(probe, condition)| {
        let sessions = &sessions;
        let recallers = &recallers;
        let builder = &builder;
        let answer_model = &answer_model;
        let judge_model = &judge_model;
        async move {
            let result = async {
                let session = sessions.get(&probe.rollout).context("session not loaded")?;
                let history = session.history(Some(probe.turn));
                let recaller = recallers.get(&condition.to_string());
                let context = builder.build(condition, &history, probe, recaller).await?;
                answer_and_judge(probe, condition, context, answer_model, judge_model).await
            }
            .await;
            (probe, condition, result)
        }
    }))
    .buffer_unordered(args.concurrency.max(1));
    let mut finished = 0;
    while let Some((probe, condition, result)) = results.next().await {
        finished += 1;
        match result {
            Ok(result) => {
                append_jsonl(&args.out, &result)?;
                println!(
                    "[{finished}/{total}] {} {condition}: {} ({} context tokens)",
                    probe.id, result.verdict, result.context_tokens
                );
            }
            Err(err) => eprintln!("[{finished}/{total}] {} {condition}: {err:#}", probe.id),
        }
    }
    Ok(())
}

fn probe_report(args: &ProbeReportArgs) -> anyhow::Result<()> {
    let results: Vec<ProbeResult> = read_jsonl(&args.results)?;
    let valid: Option<HashSet<&str>> = args.valid_only.then(|| {
        results
            .iter()
            .filter(|result| result.condition == "oracle" && result.verdict == "correct")
            .map(|result| result.probe_id.as_str())
            .collect()
    });
    let results: Vec<&ProbeResult> = results
        .iter()
        .filter(|result| {
            valid
                .as_ref()
                .is_none_or(|valid| valid.contains(result.probe_id.as_str()))
        })
        .collect();
    let mut by_condition: BTreeMap<&str, Vec<&ProbeResult>> = BTreeMap::new();
    for result in &results {
        by_condition
            .entry(result.condition.as_str())
            .or_default()
            .push(result);
    }
    println!(
        "{:<28} {:>4} {:>6} {:>8} {:>9} {:>8} {:>9} {:>8}",
        "condition", "n", "score", "correct", "ctx tok", "support", "detailed", "outdated"
    );
    for (condition, results) in &by_condition {
        let n = results.len() as f64;
        let mean =
            |value: fn(&ProbeResult) -> f64| results.iter().map(|r| value(r)).sum::<f64>() / n;
        println!(
            "{condition:<28} {:>4} {:>6.3} {:>8.3} {:>9.0} {:>8.3} {:>9.3} {:>8}",
            results.len(),
            mean(ProbeResult::score),
            mean(|result| f64::from(u8::from(result.verdict == "correct"))),
            mean(|result| f64::from(result.context_tokens)),
            mean(|result| result.support_shown),
            mean(|result| result.support_detailed),
            results
                .iter()
                .map(|result| result.outdated_unmarked)
                .sum::<usize>(),
        );
    }
    let categories: Vec<&str> = {
        let mut categories: Vec<&str> = results
            .iter()
            .map(|result| result.category.as_str())
            .collect();
        categories.sort_unstable();
        categories.dedup();
        categories
    };
    println!(
        "
score by category"
    );
    print!("{:<28}", "condition");
    for category in &categories {
        print!(" {category:>11}");
    }
    println!();
    for (condition, results) in &by_condition {
        print!("{condition:<28}");
        for category in &categories {
            let scores: Vec<f64> = results
                .iter()
                .filter(|result| result.category == *category)
                .map(|result| result.score())
                .collect();
            if scores.is_empty() {
                print!(" {:>11}", "-");
            } else {
                let mean = scores.iter().sum::<f64>() / scores.len() as f64;
                print!(" {:>6.2} ({:>2})", mean, scores.len());
            }
        }
        println!();
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
