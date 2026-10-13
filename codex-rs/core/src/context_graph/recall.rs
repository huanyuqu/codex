//! Rebuilding the history the model sees by recall, in place of summary compaction.

use std::sync::Arc;

use codex_config::types::ContextMode;
use codex_context_graph::BlockKind;
use codex_context_graph::history::History;
use codex_context_graph::text::item_text;
use codex_context_graph::view::RenderOptions;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::render_view;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::ShadowModel;
use codex_features::Feature;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use tracing::warn;

use super::ContextGraphService;
use crate::compact::CompactedHistoryMetadata;
use crate::compact::build_compaction_replacement_history;
use crate::context::UserGoalUpdate;
use crate::session::context_window::context_window_token_status;
use crate::session::session::Session;
use crate::session::step_context::StepContext;

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

/// The items that replace the history, and a note for the compaction record.
struct RecalledView {
    items: Vec<ResponseItem>,
    message: String,
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
        match (self.config.mode, trigger) {
            (ContextMode::Full, _) => false,
            (_, RecallTrigger::ContextLimit | RecallTrigger::Manual)
            | (ContextMode::Recall, RecallTrigger::TurnStart) => true,
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

    /// Recalls the branch's history for `question`, or for the latest request without one.
    /// Returns `None` when everything is in the recent turns, so recall would change nothing.
    async fn build_view(&self, question: Option<&str>) -> anyhow::Result<Option<RecalledView>> {
        let snapshot = self.live.snapshot().await?;
        let history = snapshot.history(/*until_turn*/ None);
        let recent_start = history.recent_start(self.config.recent_turns);
        if !(0..recent_start).any(|index| history.recallable(index)) {
            return Ok(None);
        }
        let question = match question
            .map(str::trim)
            .filter(|question| !question.is_empty())
        {
            Some(question) => question.to_string(),
            None => last_request(&history).unwrap_or_default(),
        };
        let screen_model: Arc<dyn ShadowModel> = self.screen_model.clone();
        let read_model = self
            .read_model
            .clone()
            .map(|model| -> Arc<dyn ShadowModel> { model });
        let config = RecallConfig {
            view: ViewLimits {
                budget_tokens: self.config.recall_budget_tokens,
                recent_turns: self.config.recent_turns,
                ..ViewLimits::default()
            },
            ..RecallConfig::default()
        };
        let recall = Recaller::new(screen_model, read_model, config)
            .recall(&history, &question)
            .await?;
        let items = render_view(
            &history,
            &recall.view,
            RenderOptions {
                keep_recent_reasoning: true,
                tools_hint: true,
            },
        );

        let membership = self.live.membership();
        let recorded = match self.live.store().await {
            Ok(store) => {
                store
                    .record_view(
                        &membership.graph_id,
                        &membership.branch,
                        &question,
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
        let message = format!(
            "Recalled {} of {} blocks, about {} tokens, for the current request.",
            recall.view.entries.len(),
            history.len(),
            recall.view.tokens
        );
        Ok(Some(RecalledView { items, message }))
    }
}

/// Text of the latest user message.
fn last_request(history: &History<'_>) -> Option<String> {
    let index = (0..history.len())
        .rev()
        .find(|index| history.card(*index).kind == BlockKind::UserMessage)?;
    Some(
        history
            .block(index)
            .items
            .iter()
            .filter_map(item_text)
            .map(|text| text.text)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Rebuilds the history by recall when the session recalls and `trigger` calls for it, and
/// returns whether it did. `question` is the request to recall for; without one, the latest user
/// message is used. A failed recall leaves the history as it was, so callers can fall back to
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
    let turn_context = &step_context.turn;
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let view = match service.build_view(question).await {
        Ok(view) => view,
        Err(err) => {
            warn!("context recall failed: {err:#}");
            None
        }
    };
    let installed = match view {
        Some(view) => {
            install(sess, step_context, view).await?;
            true
        }
        None => false,
    };
    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;
    Ok(installed)
}

/// Replaces the history with `view`, the same way compaction installs its summary, so that the
/// window, persistence and resume work unchanged.
async fn install(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    view: RecalledView,
) -> CodexResult<()> {
    let world_state = sess
        .build_world_state_for_step(step_context, /*new_window*/ true)
        .await?;
    let input_goal_ids = UserGoalUpdate::message_ids(sess.clone_history().await.raw_items());
    let items = view
        .items
        .into_iter()
        .map(ResponseItemEnvelope::new)
        .collect();
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
            message: view.message,
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
