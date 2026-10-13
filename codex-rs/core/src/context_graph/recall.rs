//! Rebuilding the history the model sees by recall, in place of summary compaction.
//!
//! In `auto` mode the history is rebuilt when it fills the soft limit of the context window. In
//! `recall` mode every turn screens the history for its request: when most of what it needs is
//! already in the window, recall appends only the missing blocks, so the window keeps its prefix
//! and the provider's prompt cache; when the request moved to another topic, recall rebuilds the
//! window. A recall that fails or takes too long leaves the history as it was.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use codex_config::types::ContextMode;
use codex_context_graph::BlockKind;
use codex_context_graph::history::History;
use codex_context_graph::text::item_text;
use codex_context_graph::view::ContextView;
use codex_context_graph::view::Reason;
use codex_context_graph::view::RenderOptions;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::entry_text;
use codex_context_graph::view::render_view;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::Recall;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::ShadowModel;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
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

    /// Whether `snapshot` has history before the recent turns; without it, recall would change
    /// nothing.
    fn has_earlier_history(&self, snapshot: &BranchSnapshot) -> bool {
        let history = snapshot.history(/*until_turn*/ None);
        let recent_start = history.recent_start(self.config.recent_turns);
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
            view: ViewLimits {
                budget_tokens,
                recent_turns,
                ..ViewLimits::default()
            },
            ..RecallConfig::default()
        };
        Recaller::new(screen_model, read_model, config)
    }

    /// Recalls `history` for `question`, giving up after the configured timeout, and records the
    /// view it built.
    async fn recall(
        &self,
        history: &History<'_>,
        question: &str,
        budget_tokens: u32,
        recent_turns: usize,
    ) -> anyhow::Result<Recall> {
        let timeout = Duration::from_secs(self.config.recall_timeout_secs);
        let recall = tokio::time::timeout(
            timeout,
            self.recaller(budget_tokens, recent_turns)
                .recall(history, question),
        )
        .await
        .map_err(|_| anyhow::anyhow!("recall took longer than {}s", timeout.as_secs()))??;

        let membership = self.live.membership();
        let recorded = match self.live.store().await {
            Ok(store) => {
                store
                    .record_view(
                        &membership.graph_id,
                        &membership.branch,
                        question,
                        self.screen_model.name(),
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
        Ok(recall)
    }

    /// Answers a `context_recall` request: the earlier blocks recall picks for `question`, as
    /// text.
    pub(crate) async fn recall_for_tool(&self, question: &str) -> anyhow::Result<String> {
        let snapshot = self.snapshot().await?;
        let history = snapshot.history(/*until_turn*/ None);
        let recall = self
            .recall(
                &history,
                question,
                TOOL_RECALL_BUDGET_TOKENS,
                /*recent_turns*/ 0,
            )
            .await?;
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

/// What the window holds after it was rebuilt from `view`.
fn window_after(history: &History<'_>, view: &ContextView) -> RecalledWindow {
    RecalledWindow {
        shown: view
            .entries
            .iter()
            .map(|entry| entry.block_id.clone())
            .collect(),
        known: (0..history.len())
            .filter(|index| history.is_own(*index))
            .map(|index| history.card(index).block_id.clone())
            .collect(),
    }
}

/// How much of what `view` picked the window already holds, from 0 to 100, and the picked
/// entries it lacks.
fn coverage(
    history: &History<'_>,
    view: &ContextView,
    window: &RecalledWindow,
) -> (u8, Vec<usize>) {
    let picked: Vec<usize> = view
        .entries
        .iter()
        .filter(|entry| entry.reason != Reason::Recent)
        .map(|entry| entry.index)
        .collect();
    if picked.is_empty() {
        return (100, Vec::new());
    }
    let missing: Vec<usize> = picked
        .iter()
        .copied()
        .filter(|index| !window.holds(&history.card(*index).block_id, history.is_own(*index)))
        .collect();
    let held = picked.len() - missing.len();
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
    if !service.wants_recall(sess, step_context, trigger).await {
        return Ok(false);
    }
    let snapshot = match service.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            warn!("context recall could not read the history: {err:#}");
            return Ok(false);
        }
    };
    if !service.has_earlier_history(&snapshot) {
        return Ok(false);
    }
    let history = snapshot.history(/*until_turn*/ None);
    let question = request_text(&history, question);
    let window = service.window();
    let incremental = trigger == RecallTrigger::TurnStart
        && service.mode() == ContextMode::Recall
        && window.is_some();
    let turn_context = &step_context.turn;
    // A rebuild replaces the history the way compaction does, so clients show it as one.
    let compaction_item =
        (!incremental).then(|| TurnItem::ContextCompaction(ContextCompactionItem::new()));
    if let Some(item) = &compaction_item {
        sess.emit_turn_item_started(turn_context, item).await;
    }
    let recall = service
        .recall(
            &history,
            &question,
            service.config.recall_budget_tokens,
            service.config.recent_turns,
        )
        .await;
    let replaced = match recall {
        Err(err) => {
            warn!("context recall failed: {err:#}");
            false
        }
        Ok(recall) => {
            let append = match (&window, incremental) {
                (Some(window), true) => {
                    let (held, missing) = coverage(&history, &recall.view, window);
                    (held >= service.config.rebuild_overlap_percent).then_some(missing)
                }
                _ => None,
            };
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
    if let Some(item) = compaction_item {
        sess.emit_turn_item_completed(turn_context, item).await;
    }
    Ok(replaced)
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
        let added: HashSet<_> = missing
            .iter()
            .map(|index| history.card(*index).block_id.clone())
            .collect();
        window.shown.extend(added);
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
