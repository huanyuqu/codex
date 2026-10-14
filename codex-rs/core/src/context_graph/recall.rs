//! Rebuilding the history the model sees by recall, in place of summary compaction.
//!
//! In `auto` mode the history is rebuilt when it fills the soft limit of the context window. In
//! `recall` mode every turn screens the history for its request: when most of what it needs is
//! already in the window, recall appends only the missing blocks, so the window keeps its prefix
//! and the provider's prompt cache; when the request moved to another topic, recall rebuilds the
//! window.
//!
//! A turn waits for the recall of its request for at most `recall_wait_ms`. When the recall takes
//! longer, the turn goes on with the recall made in the background when the previous turn ended,
//! if there is one, or with summaries of the blocks lexical search ranks highest; the late recall
//! adds the blocks it picked before a later model request of the turn, without rebuilding. A turn
//! whose request is already served by the recall made when the previous turn ended, because it
//! shows the blocks lexical search ranks highest for the request, starts from it without
//! screening again. A recall that fails or takes too long leaves the history as it was.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_config::types::ContextMode;
use codex_config::types::RecallScreen;
use codex_context_graph::BlockKind;
use codex_context_graph::history::History;
use codex_context_graph::search::lexical_matches;
use codex_context_graph::text::item_text;
use codex_context_graph::text::truncate;
use codex_context_graph::view::ContextView;
use codex_context_graph::view::Reason;
use codex_context_graph::view::RenderOptions;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::entry_text;
use codex_context_graph::view::render_view;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::Recall;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::RecallDelivery;
use codex_context_graph_store::RecallFeedback;
use codex_context_graph_store::RecallStats;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::ScreenStrategy;
use codex_context_graph_store::Selection;
use codex_context_graph_store::ShadowModel;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ContextViewRecalledEntry;
use codex_protocol::protocol::ContextViewRecalledEvent;
use codex_protocol::protocol::EventMsg;
use tokio::sync::watch;
use tokio_util::task::AbortOnDropHandle;
use tracing::warn;

use super::ContextGraphService;
use super::RecalledWindow;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::build_compaction_replacement_history;
use crate::context::UserGoalUpdate;
use crate::session::context_window::context_window_token_status;
use crate::session::session::Session;
use crate::session::step_context::StepContext;

/// Tokens a `context_recall` answer may take.
const TOOL_RECALL_BUDGET_TOKENS: u32 = 12_000;
/// Blocks lexical search ranks highest for a request that an earlier recall must show to serve
/// it.
const COVERED_MATCHES: usize = 3;
/// Blocks lexical search ranks highest that a turn adds as summaries while the recall of its
/// request runs.
const QUICK_MATCHES: usize = 3;
/// Characters of a turn's last answer that the recall made for the next turn reads.
const SPECULATIVE_ANSWER_CHARS: usize = 2_000;
/// The selector named in records of views made by lexical search alone.
const LEXICAL_SELECTOR: &str = "lexical";

/// When the history may be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecallTrigger {
    /// A turn starts and its input is recorded.
    TurnStart,
    /// The context window is full, where summary compaction would run.
    ContextLimit,
    /// The user asked to compact.
    Manual,
}

/// Where a background recall stands.
#[derive(Debug, Clone)]
enum JobState {
    Running,
    Done(Arc<Selection>),
    Failed,
}

/// What a background recall was started for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobKind {
    /// The request of the turn that started it.
    Request,
    /// The turn that just ended, ahead of the next request.
    Speculative,
}

/// A recall that runs, or ran, in the background.
pub(crate) struct RecallJob {
    question: String,
    kind: JobKind,
    state: watch::Receiver<JobState>,
    /// Whether its selection reached the context window.
    applied: bool,
    _task: AbortOnDropHandle<()>,
}

impl RecallJob {
    /// The selection, once the recall chose one.
    fn selection(&self) -> Option<Arc<Selection>> {
        match &*self.state.borrow() {
            JobState::Done(selection) => Some(Arc::clone(selection)),
            JobState::Running | JobState::Failed => None,
        }
    }

    /// Waits at most `wait` for the recall to end: `Some` with its selection, or with `None`
    /// when it failed; `None` when it still runs.
    async fn wait(&mut self, wait: Duration) -> Option<Option<Arc<Selection>>> {
        let ended = tokio::time::timeout(
            wait,
            self.state
                .wait_for(|state| !matches!(state, JobState::Running)),
        )
        .await;
        match ended {
            Err(_) => None,
            Ok(Err(_)) => Some(None),
            Ok(Ok(state)) => Some(match &*state {
                JobState::Done(selection) => Some(Arc::clone(selection)),
                JobState::Running | JobState::Failed => None,
            }),
        }
    }

    /// How a selection of this recall reaches the window when a later request uses it.
    fn reused(&self) -> RecallDelivery {
        match self.kind {
            JobKind::Request => RecallDelivery::Earlier,
            JobKind::Speculative => RecallDelivery::Speculative,
        }
    }
}

/// What a turn starts from.
enum TurnSelection {
    /// A selection to show now.
    Ready {
        selection: Arc<Selection>,
        delivery: RecallDelivery,
        waited_ms: u64,
        /// Whether the recall of the request still runs, the selection being an earlier one.
        pending: bool,
    },
    /// Nothing yet: the recall of the request runs on in the background.
    Pending,
    Failed,
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

impl ContextGraphService {
    async fn wants_recall(
        &self,
        sess: &Session,
        step_context: &StepContext,
        trigger: RecallTrigger,
    ) -> bool {
        // Token budgets manage their own context windows.
        if step_context
            .turn
            .config
            .features
            .enabled(Feature::TokenBudget)
        {
            return false;
        }
        match (self.mode(), trigger) {
            (ContextMode::Full, _) => false,
            (_, RecallTrigger::ContextLimit | RecallTrigger::Manual)
            | (ContextMode::Recall, RecallTrigger::TurnStart) => true,
            // A branch referenced since the last recall enters the history at once.
            (ContextMode::Auto, RecallTrigger::TurnStart) if self.has_new_references().await => {
                true
            }
            (ContextMode::Auto, RecallTrigger::TurnStart) => {
                let status = context_window_token_status(sess, &step_context.turn).await;
                let soft_limit = i64::from(self.config.soft_limit_percent);
                status.token_limit_reached
                    || status
                        .full_context_window_limit
                        .or_else(|| step_context.turn.model_context_window())
                        .is_some_and(|window| {
                            status.active_context_tokens.saturating_mul(100)
                                >= window.saturating_mul(soft_limit)
                        })
            }
        }
    }

    async fn has_new_references(&self) -> bool {
        let Ok(count) = self.live.reference_count().await else {
            return false;
        };
        let recalled = self
            .recalled_references
            .lock()
            .ok()
            .and_then(|recalled| *recalled)
            .unwrap_or_default();
        count > recalled
    }

    async fn note_recalled_references(&self) {
        if let Ok(count) = self.live.reference_count().await
            && let Ok(mut recalled) = self.recalled_references.lock()
        {
            *recalled = Some(count);
        }
    }

    /// Whether `snapshot` has history before its `recent_turns`; without it, recall would
    /// change nothing.
    fn has_earlier_history(&self, snapshot: &BranchSnapshot, recent_turns: usize) -> bool {
        let history = snapshot.history(/*until_turn*/ None);
        let recent_start = history.recent_start(recent_turns);
        (0..history.len())
            .any(|index| history.recallable(index) && !history.is_recent(index, recent_start))
    }

    fn recaller(&self, budget_tokens: u32, recent_turns: usize) -> Recaller {
        let screen_model: Arc<dyn ShadowModel> = self.screen_model.clone();
        let read_model = self
            .read_model
            .clone()
            .map(|model| -> Arc<dyn ShadowModel> { model });
        let config = RecallConfig {
            view: self.view_limits(budget_tokens, recent_turns),
            strategy: match self.config.screen {
                RecallScreen::Listwise => ScreenStrategy::Listwise,
                RecallScreen::Pointwise => ScreenStrategy::Pointwise,
                RecallScreen::Cascade => ScreenStrategy::Cascade,
            },
            max_matches: if self.config.lexical_share_percent == 0 {
                0
            } else {
                RecallConfig::default().max_matches
            },
            ..RecallConfig::default()
        };
        let escalate_model = self
            .escalate_model
            .clone()
            .map(|model| -> Arc<dyn ShadowModel> { model });
        Recaller::new(screen_model, read_model, config)
            .with_escalation(escalate_model)
            .with_scorer(self.scorer.clone())
    }

    /// The limits of a view of `budget_tokens` that keeps `recent_turns` in full.
    fn view_limits(&self, budget_tokens: u32, recent_turns: usize) -> ViewLimits {
        ViewLimits {
            budget_tokens,
            recent_turns,
            lexical_budget_tokens: budget_tokens
                .saturating_mul(u32::from(self.config.lexical_share_percent))
                / 100,
            ..ViewLimits::default()
        }
    }

    /// Chooses the blocks of `history` before its `recent_turns` that `question` needs, giving
    /// up after the configured timeout.
    async fn select(
        &self,
        history: &History<'_>,
        question: &str,
        recent_turns: usize,
    ) -> anyhow::Result<Selection> {
        let timeout = Duration::from_secs(self.config.recall_timeout_secs);
        tokio::time::timeout(
            timeout,
            self.recaller(self.config.recall_budget_tokens, recent_turns)
                .select(history, question),
        )
        .await
        .map_err(|_| anyhow::anyhow!("recall took longer than {}s", timeout.as_secs()))?
    }

    /// The view of `history` that shows `selection` within the recall budget.
    fn plan(&self, history: &History<'_>, selection: &Selection) -> Recall {
        selection.plan(
            history,
            self.view_limits(self.config.recall_budget_tokens, self.config.recent_turns),
        )
    }

    /// Records a view that recall showed the model.
    async fn record_view(&self, question: &str, selector: &str, recall: &Recall) {
        let membership = self.live.membership();
        let recorded = match self.live.store().await {
            Ok(store) => {
                store
                    .record_view(
                        &membership.graph_id,
                        &membership.branch,
                        question,
                        selector,
                        &recall.view,
                        &recall.stats,
                    )
                    .await
            }
            Err(err) => Err(err),
        };
        if let Err(err) = recorded {
            warn!("failed to record the recalled context view: {err:#}");
        }
    }

    /// Records a view that recall showed the model, and adds it to the rollout.
    async fn announce_view(
        &self,
        sess: &Session,
        step_context: &StepContext,
        question: &str,
        selector: &str,
        recall: &Recall,
        rebuilt: bool,
    ) {
        self.record_view(question, selector, recall).await;
        sess.send_event(
            &step_context.turn,
            EventMsg::ContextViewRecalled(self.recalled_event(question, selector, recall, rebuilt)),
        )
        .await;
    }

    /// Starts recalling `question` on `snapshot` in the background. A recall for the next turn
    /// keeps one turn fewer out of its catalog, since the next turn's request makes the latest
    /// turn an earlier one.
    fn start_job(
        self: &Arc<Self>,
        snapshot: Arc<BranchSnapshot>,
        question: String,
        kind: JobKind,
    ) -> RecallJob {
        let (send, state) = watch::channel(JobState::Running);
        let service = Arc::clone(self);
        let job_question = question.clone();
        let recent_turns = match kind {
            JobKind::Request => self.config.recent_turns,
            JobKind::Speculative => self.config.recent_turns.saturating_sub(1),
        };
        let task = tokio::spawn(async move {
            let history = snapshot.history(/*until_turn*/ None);
            let state = match service.select(&history, &job_question, recent_turns).await {
                Ok(selection) => JobState::Done(Arc::new(selection)),
                Err(err) => {
                    warn!("background recall failed: {err:#}");
                    JobState::Failed
                }
            };
            let _ = send.send(state);
        });
        RecallJob {
            question,
            kind,
            state,
            applied: false,
            _task: AbortOnDropHandle::new(task),
        }
    }

    fn take_job(&self) -> Option<RecallJob> {
        self.job.lock().ok().and_then(|mut job| job.take())
    }

    fn put_job(&self, job: RecallJob) {
        if let Ok(mut slot) = self.job.lock() {
            *slot = Some(job);
        }
    }

    /// The selection of the recall of a turn's request that finished after the turn went on
    /// without it, with its question, once; later calls get `None`.
    fn take_late_selection(&self) -> Option<(String, Arc<Selection>)> {
        let mut slot = self.job.lock().ok()?;
        let job = slot.as_mut()?;
        if job.kind != JobKind::Request || job.applied {
            return None;
        }
        let selection = job.selection()?;
        job.applied = true;
        Some((job.question.clone(), selection))
    }

    /// What a turn starts from for `question`: the recall made before the request was known
    /// when it serves the request, else the recall of the request if it ends within
    /// `recall_wait_ms`, else the earlier recall while the recall of the request runs on.
    /// `window` is what the window holds when recall may append to it.
    async fn turn_selection(
        self: &Arc<Self>,
        snapshot: &Arc<BranchSnapshot>,
        history: &History<'_>,
        question: &str,
        window: Option<&RecalledWindow>,
    ) -> TurnSelection {
        let earlier = self.take_job();
        let reused = earlier
            .as_ref()
            .and_then(|job| Some((job.selection()?, job.reused())));
        if let (Some(mut job), Some((selection, delivery))) = (earlier, reused.clone())
            && serves(
                &selection,
                history,
                question,
                self.config.recent_turns,
                window,
            )
        {
            job.applied = true;
            self.put_job(job);
            return TurnSelection::Ready {
                selection,
                delivery,
                waited_ms: 0,
                pending: false,
            };
        }

        let started = Instant::now();
        let mut job = self.start_job(Arc::clone(snapshot), question.to_string(), JobKind::Request);
        let ended = job
            .wait(Duration::from_millis(self.config.recall_wait_ms))
            .await;
        let waited_ms = elapsed_ms(started);
        let chosen = match (ended, reused) {
            (Some(Some(selection)), _) => {
                job.applied = true;
                TurnSelection::Ready {
                    selection,
                    delivery: RecallDelivery::Waited,
                    waited_ms,
                    pending: false,
                }
            }
            (ended, Some((selection, delivery))) => TurnSelection::Ready {
                selection,
                delivery,
                waited_ms,
                pending: ended.is_none(),
            },
            (Some(None), None) => TurnSelection::Failed,
            (None, None) => TurnSelection::Pending,
        };
        // A recall that ends later adds its blocks between model requests, and serves the next
        // turn when nothing newer replaces it.
        self.put_job(job);
        chosen
    }

    /// The selection for `question` when the history has to change now: the recall of it that
    /// runs in the background, waited for, or a new one.
    async fn selection_now(
        &self,
        history: &History<'_>,
        question: &str,
    ) -> Option<(Arc<Selection>, u64)> {
        let started = Instant::now();
        match self.take_job() {
            Some(mut job) if job.question == question => {
                let ended = job
                    .wait(Duration::from_secs(self.config.recall_timeout_secs))
                    .await;
                job.applied = true;
                self.put_job(job);
                if let Some(Some(selection)) = ended {
                    return Some((selection, elapsed_ms(started)));
                }
            }
            Some(job) => self.put_job(job),
            None => {}
        }
        match self
            .select(history, question, self.config.recent_turns)
            .await
        {
            Ok(selection) => Some((Arc::new(selection), elapsed_ms(started))),
            Err(err) => {
                warn!("context recall failed: {err:#}");
                None
            }
        }
    }

    /// Appends summaries of the blocks lexical search ranks highest for `question` that the
    /// window lacks, so that the turn has them while the recall of its request runs.
    async fn add_quick_matches(
        &self,
        sess: &Arc<Session>,
        step_context: &Arc<StepContext>,
        history: &History<'_>,
        question: &str,
        window: &RecalledWindow,
    ) {
        if self.config.lexical_share_percent == 0 {
            return;
        }
        let recent_start = history.recent_start(self.config.recent_turns);
        let matches: Vec<_> = lexical_matches(history, question, recent_start, QUICK_MATCHES)
            .into_iter()
            .map(|index| history.card(index).block_id.clone())
            .collect();
        if matches.is_empty() {
            return;
        }
        let selection = Selection {
            question: question.to_string(),
            picks: Vec::new(),
            matches,
            stats: RecallStats {
                delivery: Some(RecallDelivery::Lexical),
                ..RecallStats::default()
            },
        };
        let recall = self.plan(history, &selection);
        let missing: Vec<usize> = recall
            .view
            .entries
            .iter()
            .filter(|entry| {
                entry.reason == Reason::Lexical
                    && !window.holds(
                        &entry.block_id,
                        history.is_own(entry.index),
                        entry.presentation.detail(),
                    )
            })
            .map(|entry| entry.index)
            .collect();
        if missing.is_empty() {
            return;
        }
        self.announce_view(
            sess,
            step_context,
            question,
            LEXICAL_SELECTOR,
            &recall,
            /*rebuilt*/ false,
        )
        .await;
        append_recalled(sess, step_context, self, history, &recall.view, &missing).await;
    }

    /// The record of a view recall built, for the rollout.
    fn recalled_event(
        &self,
        question: &str,
        selector: &str,
        recall: &Recall,
        rebuilt: bool,
    ) -> ContextViewRecalledEvent {
        let membership = self.live.membership();
        ContextViewRecalledEvent {
            graph_id: membership.graph_id.clone(),
            branch_id: membership.branch.to_string(),
            question: question.to_string(),
            selector: selector.to_string(),
            rebuilt,
            entries: recall
                .view
                .entries
                .iter()
                .map(|entry| ContextViewRecalledEntry {
                    block_id: entry.block_id.to_string(),
                    presentation: entry.presentation.name().to_string(),
                    reason: entry.reason.name().to_string(),
                    tokens: entry.tokens,
                })
                .collect(),
            tokens: recall.view.tokens,
            budget_tokens: recall.view.budget_tokens,
            latency_ms: recall.stats.latency_ms,
        }
    }

    /// The view recall would build for `question` now, without changing the history or
    /// recording the view.
    pub(crate) async fn preview_recall(&self, question: &str) -> anyhow::Result<RecallPreview> {
        let snapshot = self.snapshot().await?;
        let history = snapshot.history(/*until_turn*/ None);
        let recall = self
            .recaller(self.config.recall_budget_tokens, self.config.recent_turns)
            .recall(&history, question)
            .await?;
        let titles = recall
            .view
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.block_id.clone(),
                    history.card(entry.index).title.trim().to_string(),
                )
            })
            .collect();
        Ok(RecallPreview {
            selector: self.screen_model.name().to_string(),
            recall,
            titles,
        })
    }

    /// Asks for the history to be rebuilt by recall before the next turn, and counts the
    /// request.
    pub(crate) async fn request_recall(&self) -> anyhow::Result<()> {
        self.request_rebuild();
        let membership = self.live.membership();
        self.live
            .store()
            .await?
            .record_recall_feedback(
                &membership.graph_id,
                &membership.branch,
                RecallFeedback::Requested,
                /*block*/ None,
            )
            .await
    }

    /// Counts that the model read the block at `index` of `history` with `context_expand`, and
    /// whether recall had left it out of the window.
    pub(crate) async fn note_expanded(&self, history: &History<'_>, index: usize) {
        let block = history.card(index).block_id.clone();
        let missed = self
            .window()
            .is_some_and(|window| !window.holds(&block, history.is_own(index), /*detail*/ 1));
        let membership = self.live.membership();
        let store = match self.live.store().await {
            Ok(store) => store,
            Err(err) => {
                warn!("could not count an expanded block: {err:#}");
                return;
            }
        };
        let mut kinds = vec![RecallFeedback::Expanded];
        if missed {
            kinds.push(RecallFeedback::Missed);
        }
        for kind in kinds {
            if let Err(err) = store
                .record_recall_feedback(
                    &membership.graph_id,
                    &membership.branch,
                    kind,
                    Some(&block),
                )
                .await
            {
                warn!("could not count an expanded block: {err:#}");
            }
        }
    }

    /// Answers a `context_recall` request: the earlier blocks recall picks for `question`, as
    /// text.
    pub(crate) async fn recall_for_tool(&self, question: &str) -> anyhow::Result<String> {
        let snapshot = self.snapshot().await?;
        let history = snapshot.history(/*until_turn*/ None);
        let selection = self.select(&history, question, /*recent_turns*/ 0).await?;
        let recall = selection.plan(
            &history,
            self.view_limits(TOOL_RECALL_BUDGET_TOKENS, /*recent_turns*/ 0),
        );
        self.record_view(question, self.screen_model.name(), &recall)
            .await;
        let picked: Vec<String> = recall
            .view
            .entries
            .iter()
            .filter(|entry| entry.reason != Reason::Recent && entry.reason != Reason::TaskStart)
            .map(|entry| entry_text(&history, entry))
            .collect();
        if picked.is_empty() {
            return Ok(format!(
                "No earlier part of this conversation is needed for: {}",
                question.trim()
            ));
        }
        Ok(picked.join("\n\n"))
    }
}

/// The view recall would build for a request, with the titles of the blocks it shows.
pub(crate) struct RecallPreview {
    pub(crate) selector: String,
    pub(crate) recall: Recall,
    pub(crate) titles: std::collections::HashMap<codex_context_graph::BlockId, String>,
}

/// The request to recall for: `question`, or the latest user message without one.
fn request_text(history: &History<'_>, question: Option<&str>) -> String {
    if let Some(question) = question
        .map(str::trim)
        .filter(|question| !question.is_empty())
    {
        return question.to_string();
    }
    (0..history.len())
        .rev()
        .find(|index| history.card(*index).kind == BlockKind::UserMessage && history.is_own(*index))
        .map(|index| {
            history
                .block(index)
                .items
                .iter()
                .filter_map(item_text)
                .map(|text| text.text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether `selection`, made before `question` was asked, serves it too: it shows, or the
/// window holds, each block that lexical search ranks highest for the question among the blocks
/// before the `recent_turns`. A question without lexical matches, such as "go on", is served.
fn serves(
    selection: &Selection,
    history: &History<'_>,
    question: &str,
    recent_turns: usize,
    window: Option<&RecalledWindow>,
) -> bool {
    let recent_start = history.recent_start(recent_turns);
    lexical_matches(history, question, recent_start, COVERED_MATCHES)
        .into_iter()
        .all(|index| {
            let block = &history.card(index).block_id;
            selection.shows(block)
                || window.is_some_and(|window| {
                    window.holds(block, history.is_own(index), /*detail*/ 1)
                })
        })
}

/// What the window holds after it was rebuilt from `view`.
fn window_after(history: &History<'_>, view: &ContextView) -> RecalledWindow {
    RecalledWindow {
        shown: view
            .entries
            .iter()
            .map(|entry| (entry.block_id.clone(), entry.presentation.detail()))
            .collect(),
        known: (0..history.len())
            .filter(|index| history.is_own(*index))
            .map(|index| history.card(index).block_id.clone())
            .collect(),
    }
}

/// How much of what `view` picked the window already holds, from 0 to 100, and the entries it
/// lacks or shows with less detail. Lexical matches are added when missing but do not count
/// toward how much is held, since no selector picked them.
fn coverage(
    history: &History<'_>,
    view: &ContextView,
    window: &RecalledWindow,
) -> (u8, Vec<usize>) {
    let wanted: Vec<_> = view
        .entries
        .iter()
        .filter(|entry| entry.reason != Reason::Recent)
        .collect();
    let missing: Vec<usize> = wanted
        .iter()
        .filter(|entry| {
            !window.holds(
                &entry.block_id,
                history.is_own(entry.index),
                entry.presentation.detail(),
            )
        })
        .map(|entry| entry.index)
        .collect();
    let picked: Vec<usize> = wanted
        .iter()
        .filter(|entry| entry.reason != Reason::Lexical)
        .map(|entry| entry.index)
        .collect();
    if picked.is_empty() {
        return (100, missing);
    }
    let held = picked
        .iter()
        .filter(|index| !missing.contains(index))
        .count();
    let percent = u8::try_from(held * 100 / picked.len()).unwrap_or(100);
    (percent, missing)
}

/// Rebuilds the history by recall when the session recalls and `trigger` calls for it, and
/// returns whether it replaced the history. `question` is the request to recall for; without
/// one, the latest user message is used. In `recall` mode a turn whose request mostly needs
/// what the window holds gets the missing blocks appended instead, which keeps the history and
/// returns false. A failed recall leaves the history as it was, so callers can fall back to
/// summary compaction.
pub(crate) async fn recall_into_history(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    question: Option<&str>,
    trigger: RecallTrigger,
) -> CodexResult<bool> {
    let Some(service) = sess.services.context_graph.clone() else {
        return Ok(false);
    };
    // A rebuild the user asked for runs whatever the mode.
    let requested = trigger == RecallTrigger::TurnStart && service.take_rebuild_request();
    if !requested && !service.wants_recall(sess, step_context, trigger).await {
        return Ok(false);
    }
    let snapshot = match service.snapshot().await {
        Ok(snapshot) => Arc::new(snapshot),
        Err(err) => {
            warn!("context recall could not read the history: {err:#}");
            return Ok(false);
        }
    };
    if !service.has_earlier_history(&snapshot, service.config.recent_turns) {
        return Ok(false);
    }
    let history = snapshot.history(/*until_turn*/ None);
    let question = request_text(&history, question);
    let window = service.window();
    let incremental = trigger == RecallTrigger::TurnStart
        && !requested
        && service.mode() == ContextMode::Recall
        && window.is_some();
    let turn_context = &step_context.turn;
    // A rebuild replaces the history the way compaction does, so clients show it as one.
    let compaction_item =
        (!incremental).then(|| TurnItem::ContextCompaction(ContextCompactionItem::new()));
    if let Some(item) = &compaction_item {
        sess.emit_turn_item_started(turn_context, item).await;
    }
    // A turn waits only briefly; when the history has to change now, recall is waited for.
    let chosen = if trigger == RecallTrigger::TurnStart && !requested {
        service
            .turn_selection(
                &snapshot,
                &history,
                &question,
                window.as_ref().filter(|_| incremental),
            )
            .await
    } else {
        match service.selection_now(&history, &question).await {
            Some((selection, waited_ms)) => TurnSelection::Ready {
                selection,
                delivery: RecallDelivery::Waited,
                waited_ms,
                pending: false,
            },
            None => TurnSelection::Failed,
        }
    };
    let pending = matches!(
        chosen,
        TurnSelection::Pending | TurnSelection::Ready { pending: true, .. }
    );
    let replaced = match chosen {
        TurnSelection::Failed | TurnSelection::Pending => false,
        TurnSelection::Ready {
            selection,
            delivery,
            waited_ms,
            pending: _,
        } => {
            let mut recall = service.plan(&history, &selection);
            recall.stats.delivery = Some(delivery);
            recall.stats.waited_ms = waited_ms;
            let append = match (&window, incremental) {
                (Some(window), true) => {
                    let (held, missing) = coverage(&history, &recall.view, window);
                    (held >= service.config.rebuild_overlap_percent).then_some(missing)
                }
                _ => None,
            };
            service
                .announce_view(
                    sess,
                    step_context,
                    &question,
                    service.screen_model.name(),
                    &recall,
                    append.is_none(),
                )
                .await;
            match append {
                Some(missing) => {
                    append_recalled(
                        sess,
                        step_context,
                        &service,
                        &history,
                        &recall.view,
                        &missing,
                    )
                    .await;
                    false
                }
                None => {
                    install(sess, step_context, &history, &recall).await?;
                    service.set_window(Some(window_after(&history, &recall.view)));
                    service.note_recalled_references().await;
                    true
                }
            }
        }
    };
    // While the recall of the request runs, the blocks lexical search ranks highest stand in.
    if pending && let Some(window) = service.window() {
        service
            .add_quick_matches(sess, step_context, &history, &question, &window)
            .await;
    }
    if let Some(item) = compaction_item {
        sess.emit_turn_item_completed(turn_context, item).await;
    }
    Ok(replaced)
}

/// Adds the blocks that the recall of the turn's request picked, when it finished after the
/// turn went on without it and the window lacks them. Called before each model request; the
/// history is appended to, never rebuilt, so the window keeps its prefix.
pub(crate) async fn add_finished_recall(sess: &Arc<Session>, step_context: &Arc<StepContext>) {
    let Some(service) = sess.services.context_graph.clone() else {
        return;
    };
    // While the window holds the whole history, it lacks nothing.
    let Some(window) = service.window() else {
        return;
    };
    let Some((question, selection)) = service.take_late_selection() else {
        return;
    };
    let snapshot = match service.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            warn!("context recall could not read the history: {err:#}");
            return;
        }
    };
    let history = snapshot.history(/*until_turn*/ None);
    let mut recall = service.plan(&history, &selection);
    recall.stats.delivery = Some(RecallDelivery::Deferred);
    let (_, missing) = coverage(&history, &recall.view, &window);
    service
        .announce_view(
            sess,
            step_context,
            &question,
            service.screen_model.name(),
            &recall,
            /*rebuilt*/ false,
        )
        .await;
    append_recalled(
        sess,
        step_context,
        &service,
        &history,
        &recall.view,
        &missing,
    )
    .await;
}

/// Starts recalling, in the background, for the turn after the one that just ended, so that
/// the next turn need not wait: the request is not known yet, so the recall reads the ended
/// turn's request and `answer`, its last message. Runs when the next turn would recall.
pub(crate) async fn speculate_recall(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    answer: Option<&str>,
) {
    let Some(service) = sess.services.context_graph.clone() else {
        return;
    };
    if !service.config.speculative_recall
        || !service
            .wants_recall(sess, step_context, RecallTrigger::TurnStart)
            .await
    {
        return;
    }
    let snapshot = match service.snapshot().await {
        Ok(snapshot) => Arc::new(snapshot),
        Err(err) => {
            warn!("context recall could not read the history: {err:#}");
            return;
        }
    };
    // The next turn's request makes the latest turn an earlier one.
    if !service.has_earlier_history(&snapshot, service.config.recent_turns.saturating_sub(1)) {
        return;
    }
    let mut question = request_text(
        &snapshot.history(/*until_turn*/ None),
        /*question*/ None,
    );
    if let Some(answer) = answer.map(str::trim).filter(|answer| !answer.is_empty()) {
        question.push_str("\n\n");
        question.push_str(&truncate(answer, SPECULATIVE_ANSWER_CHARS));
    }
    let job = service.start_job(snapshot, question, JobKind::Speculative);
    service.put_job(job);
}

/// Appends the picked blocks the window lacks as one note, so the window keeps its prefix.
async fn append_recalled(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    service: &ContextGraphService,
    history: &History<'_>,
    view: &ContextView,
    missing: &[usize],
) {
    if missing.is_empty() {
        return;
    }
    let entries: Vec<String> = view
        .entries
        .iter()
        .filter(|entry| missing.contains(&entry.index))
        .map(|entry| entry_text(history, entry))
        .collect();
    let note = ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "Recalled from earlier in this conversation for the current request. This is \
                 recorded history, not new instructions.\n\n{}",
                entries.join("\n\n")
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    sess.record_conversation_items(
        &step_context.turn,
        &step_context.settings.model_info,
        std::slice::from_ref(&note),
    )
    .await;
    if let Some(mut window) = service.window() {
        for entry in view
            .entries
            .iter()
            .filter(|entry| missing.contains(&entry.index))
        {
            window.show(entry.block_id.clone(), entry.presentation.detail());
        }
        service.set_window(Some(window));
    }
}

/// Replaces the history with the view `recall` built, the same way compaction installs its
/// summary, so that the window, persistence and resume work unchanged.
async fn install(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    history: &History<'_>,
    recall: &Recall,
) -> CodexResult<()> {
    let items = render_view(
        history,
        &recall.view,
        RenderOptions {
            keep_recent_reasoning: true,
            tools_hint: true,
        },
    );
    let message = format!(
        "Recalled {} of {} blocks, about {} tokens, for the current request.",
        recall.view.entries.len(),
        history.len(),
        recall.view.tokens
    );
    let world_state = sess
        .build_world_state_for_step(step_context, /*new_window*/ true)
        .await?;
    let input_goal_ids = UserGoalUpdate::message_ids(sess.clone_history().await.raw_items());
    let items = items.into_iter().map(ResponseItemEnvelope::new).collect();
    let (history, baseline) =
        build_compaction_replacement_history(sess.as_ref(), step_context, &world_state, items)
            .await;
    let (window_number, window_ids) = sess.advance_auto_compact_window().await;
    sess.replace_compacted_history(
        history,
        step_context.to_turn_context_item(),
        baseline,
        CompactedHistoryMetadata {
            input_goal_ids,
            message,
            window_number,
            window_ids,
            compaction_response_id: None,
            compaction_model_hash: step_context.turn.model_info().comp_hash.clone(),
            reviewer_compaction_hash: None,
        },
    )
    .await;
    sess.recompute_token_usage(&step_context.turn).await;
    Ok(())
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
