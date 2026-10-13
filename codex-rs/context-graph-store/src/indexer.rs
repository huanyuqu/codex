//! The shadow indexer: writes a card for every new block of a branch, in the background.
//!
//! Every block gets its extracted card right away. When an indexing model is configured, blocks
//! are also collected into batches, closed after a number of blocks, a wait, or the end of a
//! turn, and the model's answer replaces the extracted titles and summaries and adds claims and
//! links. Blocks that a request fails or that its answer misses are sent once more; after that
//! they keep their extracted cards, so indexing never blocks or breaks the session it serves.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_context_graph::BlockId;
use codex_context_graph::BlockIndexer;
use codex_context_graph::BlockKind;
use codex_context_graph::BranchId;
use codex_context_graph::IndexUpdate;
use codex_context_graph::RecorderEvent;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::CARD_VERSION;
use codex_context_graph::card::FileStamp;
use codex_context_graph::card_prompt::CardPrompt;
use codex_context_graph::card_prompt::PromptLimits;
use codex_context_graph::card_prompt::build_card_prompt;
use codex_context_graph::card_prompt::parse_card_answer;
use codex_protocol::models::ResponseItem;
use codex_secrets::redact_secrets;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::warn;

use crate::LocalContextGraphStore;
use crate::cards::IndexRequest;
use crate::model::ModelCompletion;
use crate::model::ShadowModel;

/// Cards kept in memory for the catalog of later prompts.
const MAX_RECENT_CARDS: usize = 200;

/// Requests that may carry one block: its first batch and one retry.
const MAX_ATTEMPTS: u32 = 2;

/// How the indexer batches and limits model requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerConfig {
    /// Blocks per model request.
    pub batch_blocks: usize,
    /// How long the first block of a batch may wait for the batch to fill.
    pub batch_wait: Duration,
    pub max_concurrent_requests: usize,
    /// Model requests one indexer may make; `None` for no limit.
    pub max_requests: Option<u64>,
    pub limits: PromptLimits,
    /// How long a session must be idle after a turn before its turns are consolidated into
    /// episode and branch cards; `None` to never consolidate.
    pub consolidate_after: Option<Duration>,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            batch_blocks: 8,
            batch_wait: Duration::from_secs(15),
            max_concurrent_requests: 2,
            max_requests: Some(500),
            limits: PromptLimits::default(),
            consolidate_after: Some(Duration::from_secs(60)),
        }
    }
}

struct Batch {
    prompt: CardPrompt,
    entries: Vec<(BlockCard, Vec<ResponseItem>)>,
    attempt: u32,
}

struct BatchResult {
    batch: Batch,
    completion: anyhow::Result<ModelCompletion>,
    latency: Duration,
}

/// Indexes one branch of one graph. Feed it the recorder's events in order.
pub struct ShadowIndexer {
    store: LocalContextGraphStore,
    graph_id: String,
    branch: BranchId,
    model: Option<Arc<dyn ShadowModel>>,
    config: IndexerConfig,
    indexer: BlockIndexer,
    recent: VecDeque<BlockCard>,
    pending: Vec<(BlockCard, Vec<ResponseItem>)>,
    pending_since: Option<Instant>,
    in_flight: JoinSet<BatchResult>,
    requests_started: u64,
    /// When the last turn ended, while its turns wait to be consolidated.
    unconsolidated_since: Option<Instant>,
}

impl ShadowIndexer {
    pub fn new(
        store: LocalContextGraphStore,
        graph_id: impl Into<String>,
        branch: BranchId,
        model: Option<Arc<dyn ShadowModel>>,
        config: IndexerConfig,
    ) -> Self {
        Self {
            store,
            graph_id: graph_id.into(),
            branch,
            model,
            config,
            indexer: BlockIndexer::new(),
            recent: VecDeque::new(),
            pending: Vec::new(),
            pending_since: None,
            in_flight: JoinSet::new(),
            requests_started: 0,
            unconsolidated_since: None,
        }
    }

    /// Indexes one recorder event, starting a model request when a batch is complete.
    pub async fn observe(&mut self, event: RecorderEvent) -> anyhow::Result<()> {
        match self.indexer.observe(event) {
            Some(IndexUpdate::Block { card, items }) => {
                let mut card = *card;
                redact_card(&mut card);
                card.file_stamps = file_stamps(&card.entities.files_read);
                self.store.upsert_card(&self.graph_id, &card).await?;
                if self.wants_model(&card).await? {
                    self.pending.push((card.clone(), items));
                    self.pending_since.get_or_insert_with(Instant::now);
                }
                self.remember(card);
                if self.pending.len() >= self.config.batch_blocks.max(1) {
                    self.flush().await;
                }
            }
            Some(IndexUpdate::TurnEnded { card }) => {
                if let Some(card) = card {
                    self.store
                        .upsert_turn_card(&self.graph_id, &self.branch, &card)
                        .await?;
                }
                self.flush().await;
                self.unconsolidated_since = Some(Instant::now());
            }
            None => {}
        }
        Ok(())
    }

    /// Starts a model request for the blocks waiting in the current batch.
    pub async fn flush(&mut self) {
        self.pending_since = None;
        if self.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        if self.model.is_none() || self.budget_spent() {
            return;
        }
        while self.in_flight.len() >= self.config.max_concurrent_requests.max(1) {
            self.complete_one().await;
        }
        self.start_batch(pending, /*attempt*/ 1);
    }

    /// Starts a model request for `entries`, with the cards recorded before them as the catalog.
    fn start_batch(&mut self, entries: Vec<(BlockCard, Vec<ResponseItem>)>, attempt: u32) {
        let Some(model) = self.model.clone() else {
            return;
        };
        if entries.is_empty() || self.budget_spent() {
            return;
        }
        let batch_ids: Vec<&BlockId> = entries.iter().map(|(card, _)| &card.block_id).collect();
        // A retried block is older than the cards recorded since, so they stay out of its
        // catalog.
        let end = self
            .recent
            .iter()
            .rposition(|card| batch_ids.contains(&&card.block_id))
            .map_or(self.recent.len(), |last| last + 1);
        let earlier: Vec<&BlockCard> = self
            .recent
            .iter()
            .take(end)
            .filter(|card| !batch_ids.contains(&&card.block_id))
            .collect();
        let prompt_entries: Vec<(&BlockCard, &[ResponseItem])> = entries
            .iter()
            .map(|(card, items)| (card, items.as_slice()))
            .collect();
        let prompt = build_card_prompt(&prompt_entries, &earlier, self.config.limits);
        let batch = Batch {
            prompt,
            entries,
            attempt,
        };
        self.requests_started += 1;
        self.in_flight.spawn(async move {
            let started = Instant::now();
            let completion = model.complete(&batch.prompt.prompt).await;
            BatchResult {
                batch,
                completion,
                latency: started.elapsed(),
            }
        });
    }

    /// Flushes the last batch and waits for every request to finish.
    pub async fn finish(mut self) {
        self.flush().await;
        while !self.in_flight.is_empty() {
            self.complete_one().await;
        }
    }

    /// Indexes `events` until the channel closes, closing batches that waited
    /// [`IndexerConfig::batch_wait`], then waits for the last requests.
    pub async fn run(mut self, mut events: mpsc::UnboundedReceiver<RecorderEvent>) {
        loop {
            let deadline = self
                .pending_since
                .map(|since| tokio::time::Instant::from_std(since + self.config.batch_wait));
            // Consolidation waits until the cards of the last turn are written.
            let idle = self
                .unconsolidated_since
                .filter(|_| self.pending.is_empty() && self.in_flight.is_empty())
                .zip(self.config.consolidate_after)
                .map(|(since, after)| tokio::time::Instant::from_std(since + after));
            let next = tokio::select! {
                event = events.recv() => {
                    if self.unconsolidated_since.is_some() {
                        self.unconsolidated_since = Some(Instant::now());
                    }
                    Next::Event(event)
                }
                () = sleep_until(deadline), if deadline.is_some() => Next::Flush,
                () = sleep_until(idle), if idle.is_some() => Next::Idle,
                Some(result) = self.in_flight.join_next(), if !self.in_flight.is_empty() => {
                    Next::Done(result)
                }
            };
            match next {
                Next::Event(Some(event)) => {
                    if let Err(err) = self.observe(event).await {
                        warn!("context graph indexing failed: {err:#}");
                    }
                }
                Next::Event(None) => break,
                Next::Flush => self.flush().await,
                Next::Idle => self.consolidate().await,
                Next::Done(result) => self.handle(result).await,
            }
        }
        self.finish().await;
    }

    /// Sums up the branch's settled turns, with the model while the request budget lasts.
    async fn consolidate(&mut self) {
        self.unconsolidated_since = None;
        let model = self.model.clone().filter(|_| !self.budget_spent());
        match self
            .store
            .consolidate_branch(&self.graph_id, &self.branch, model.as_deref())
            .await
        {
            Ok(done) => self.requests_started += done.requests,
            Err(err) => warn!("context graph consolidation failed: {err:#}"),
        }
    }

    fn budget_spent(&self) -> bool {
        self.config
            .max_requests
            .is_some_and(|max| self.requests_started >= max)
    }

    async fn wants_model(&self, card: &BlockCard) -> anyhow::Result<bool> {
        if self.model.is_none() || !card.wants_model() {
            return Ok(false);
        }
        // A resumed session replays blocks that may already have model cards.
        Ok(!self
            .store
            .has_model_card(&self.graph_id, &card.block_id, CARD_VERSION)
            .await?)
    }

    fn remember(&mut self, card: BlockCard) {
        if card.kind == BlockKind::ContextFragment {
            return;
        }
        if self.recent.len() >= MAX_RECENT_CARDS {
            self.recent.pop_front();
        }
        self.recent.push_back(card);
    }

    async fn complete_one(&mut self) {
        if let Some(result) = self.in_flight.join_next().await {
            self.handle(result).await;
        }
    }

    async fn handle(&mut self, result: Result<BatchResult, tokio::task::JoinError>) {
        let BatchResult {
            batch,
            completion,
            latency,
        } = match result {
            Ok(result) => result,
            Err(err) => {
                warn!("context graph indexing request panicked: {err}");
                return;
            }
        };
        let Some(model) = &self.model else {
            return;
        };
        let mut request = IndexRequest {
            indexer: model.name().to_string(),
            blocks: u32::try_from(batch.entries.len()).unwrap_or(u32::MAX),
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            ..IndexRequest::default()
        };
        let cards = match completion {
            Ok(completion) => {
                request.input_tokens = completion.input_tokens;
                request.cached_input_tokens = completion.cached_input_tokens;
                request.output_tokens = completion.output_tokens;
                parse_card_answer(&batch.prompt, &completion.text).map_err(anyhow::Error::from)
            }
            Err(err) => Err(err),
        };
        let Batch {
            entries: mut unanswered,
            attempt,
            ..
        } = batch;
        let total = unanswered.len();
        match cards {
            Ok(answer) => {
                if !answer.missing.is_empty() {
                    request.error = Some(format!(
                        "answer missed {} of {total} blocks",
                        answer.missing.len()
                    ));
                }
                for model_card in answer.cards {
                    let Some(position) = unanswered
                        .iter()
                        .position(|(card, _)| card.block_id == model_card.block_id)
                    else {
                        continue;
                    };
                    let (mut card, _) = unanswered.remove(position);
                    model_card.apply_to(&mut card, &request.indexer);
                    redact_card(&mut card);
                    if let Err(err) = self.store.upsert_card(&self.graph_id, &card).await {
                        warn!("context graph card write failed: {err:#}");
                    }
                    if let Some(recent) = self
                        .recent
                        .iter_mut()
                        .find(|recent| recent.block_id == card.block_id)
                    {
                        *recent = card;
                    }
                }
            }
            Err(err) => {
                warn!("context graph indexing request failed: {err:#}");
                request.error = Some(format!("{err:#}"));
            }
        }
        if let Err(err) = self
            .store
            .record_index_request(&self.graph_id, &request)
            .await
        {
            warn!("context graph index accounting failed: {err:#}");
        }
        if attempt < MAX_ATTEMPTS {
            self.start_batch(unanswered, attempt + 1);
        }
    }
}

enum Next {
    Event(Option<RecorderEvent>),
    Flush,
    Idle,
    Done(Result<BatchResult, tokio::task::JoinError>),
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The current state of the files at `paths` that exist; relative paths, whose directory is
/// unknown, are skipped.
pub fn file_stamps(paths: &[String]) -> Vec<FileStamp> {
    paths
        .iter()
        .filter(|path| Path::new(path).is_absolute())
        .filter_map(|path| file_stamp(path))
        .collect()
}

fn file_stamp(path: &str) -> Option<FileStamp> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_ms = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(FileStamp {
        path: path.to_string(),
        modified_ms: u64::try_from(modified_ms).unwrap_or(u64::MAX),
        len: metadata.len(),
    })
}

/// Whether a file a block read changed since the block saw it, or was removed.
pub fn file_changed(stamp: &FileStamp) -> bool {
    file_stamp(&stamp.path).is_none_or(|now| now != *stamp)
}

/// Removes secrets from every free-text field of `card`.
fn redact_card(card: &mut BlockCard) {
    let redact = |text: &mut String| *text = redact_secrets(std::mem::take(text));
    redact(&mut card.title);
    redact(&mut card.summary);
    for command in &mut card.entities.commands {
        redact(command);
    }
    for error in &mut card.entities.errors {
        redact(error);
    }
    for url in &mut card.entities.urls {
        redact(url);
    }
    for claim in &mut card.claims {
        redact(&mut claim.text);
    }
}

#[cfg(test)]
#[path = "indexer_tests.rs"]
mod tests;
