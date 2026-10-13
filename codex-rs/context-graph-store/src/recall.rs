//! Recall: choosing, for one request, the blocks of a branch's history its model should see.
//!
//! A screening model reads the cards of the whole history and picks blocks, most important
//! first, with how much of each to show, and names the blocks its cards cannot decide. A reading
//! model then reads those blocks, and large picked blocks, in full, and decides for each whether
//! to show it whole, an excerpt or a summary, or to leave it out. The picks become a
//! [`ContextView`] that fits the token budget.

use std::sync::Arc;
use std::time::Instant;

use codex_context_graph::history::History;
use codex_context_graph::read::ReadLimits;
use codex_context_graph::read::build_read_prompts;
use codex_context_graph::read::parse_read_answer;
use codex_context_graph::screen::ScreenLimits;
use codex_context_graph::screen::build_screen_prompt;
use codex_context_graph::screen::parse_screen_answer;
use codex_context_graph::view::ContextView;
use codex_context_graph::view::Pick;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::plan_view;
use futures::future::join_all;
use serde::Deserialize;
use serde::Serialize;

use crate::model::ModelCompletion;
use crate::model::ShadowModel;

/// How recall screens, reads and budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallConfig {
    pub view: ViewLimits,
    pub screen: ScreenLimits,
    pub read: ReadLimits,
    /// Picked blocks shown in full that are larger than this are read, so that an excerpt may
    /// replace them.
    pub read_over_tokens: u32,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            view: ViewLimits::default(),
            screen: ScreenLimits::default(),
            read: ReadLimits::default(),
            read_over_tokens: 3_000,
        }
    }
}

/// Token usage and latency of one model request recall made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCall {
    pub model: String,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ModelCall {
    fn new(model: &dyn ShadowModel, started: Instant) -> Self {
        Self {
            model: model.name().to_string(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..Self::default()
        }
    }

    fn with_usage(mut self, completion: &ModelCompletion) -> Self {
        self.input_tokens = completion.input_tokens;
        self.cached_input_tokens = completion.cached_input_tokens;
        self.output_tokens = completion.output_tokens;
        self
    }
}

/// What one recall did and cost.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallStats {
    /// Blocks in the screening catalog.
    pub candidates: usize,
    /// Whether the history was cut down by lexical search to fit the catalog.
    pub prefiltered: bool,
    pub catalog_tokens: u32,
    pub picked: usize,
    pub uncertain: usize,
    pub read: usize,
    pub screen: Option<ModelCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<ModelCall>,
    pub latency_ms: u64,
}

/// The view a recall built, with its stats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recall {
    pub view: ContextView,
    pub stats: RecallStats,
}

/// Screens and reads with the shadow agent's models.
pub struct Recaller {
    screen_model: Arc<dyn ShadowModel>,
    read_model: Option<Arc<dyn ShadowModel>>,
    config: RecallConfig,
}

impl Recaller {
    /// A recaller that screens with `screen_model` and, when `read_model` is set, reads the
    /// blocks screening could not decide.
    pub fn new(
        screen_model: Arc<dyn ShadowModel>,
        read_model: Option<Arc<dyn ShadowModel>>,
        config: RecallConfig,
    ) -> Self {
        Self {
            screen_model,
            read_model,
            config,
        }
    }

    pub fn config(&self) -> &RecallConfig {
        &self.config
    }

    /// Builds the view of `history` for `question`. Fails when screening fails; a failed
    /// reading leaves the screening picks as they were.
    pub async fn recall(&self, history: &History<'_>, question: &str) -> anyhow::Result<Recall> {
        let started = Instant::now();
        let recent_start = history.recent_start(self.config.view.recent_turns);
        let prompt = build_screen_prompt(history, question, recent_start, self.config.screen);
        let mut stats = RecallStats {
            candidates: prompt.candidates.len(),
            prefiltered: prompt.prefiltered,
            catalog_tokens: prompt.catalog_tokens,
            ..RecallStats::default()
        };

        let screen_started = Instant::now();
        let completion = self.screen_model.complete(&prompt.prompt).await?;
        stats.screen = Some(
            ModelCall::new(self.screen_model.as_ref(), screen_started).with_usage(&completion),
        );
        let answer = parse_screen_answer(&prompt, &completion.text)?;
        let mut picks = answer.picks;
        stats.picked = picks.len();
        stats.uncertain = answer.uncertain.len();

        let large: Vec<usize> = picks
            .iter()
            .filter(|pick| {
                pick.presentation == Presentation::Full
                    && pick.index < recent_start
                    && history.tokens(pick.index) > self.config.read_over_tokens
            })
            .map(|pick| pick.index)
            .collect();
        let to_read: Vec<usize> = answer.uncertain.iter().chain(&large).copied().collect();
        match &self.read_model {
            Some(read_model) if !to_read.is_empty() => {
                stats.read = to_read.len();
                let decided = self
                    .read(read_model.as_ref(), history, question, &to_read, &mut stats)
                    .await;
                apply_reading(&mut picks, &answer.uncertain, decided);
            }
            // Without a reader, blocks screening could not decide are summarized.
            _ => picks.extend(answer.uncertain.iter().map(|index| Pick {
                index: *index,
                presentation: Presentation::Summary,
                note: Some("not read".to_string()),
            })),
        }

        let view = plan_view(history, &picks, self.config.view);
        stats.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(Recall { view, stats })
    }

    /// Reads `blocks` in parallel requests and returns each decided block with its decision.
    async fn read(
        &self,
        model: &dyn ShadowModel,
        history: &History<'_>,
        question: &str,
        blocks: &[usize],
        stats: &mut RecallStats,
    ) -> Vec<(usize, Option<Presentation>, Option<String>)> {
        let prompts = build_read_prompts(history, question, blocks, self.config.read);
        let answers = join_all(prompts.iter().map(|prompt| async move {
            let started = Instant::now();
            let completion = model.complete(&prompt.prompt).await;
            (prompt, started, completion)
        }))
        .await;
        let mut decided = Vec::new();
        for (prompt, started, completion) in answers {
            let call = ModelCall::new(model, started);
            let call = match completion {
                Ok(completion) => {
                    let call = call.with_usage(&completion);
                    match parse_read_answer(prompt, &completion.text) {
                        Ok(answer) => {
                            decided.extend(answer.decisions.into_iter().map(|decision| {
                                (decision.index, decision.presentation, decision.note)
                            }));
                            call
                        }
                        Err(err) => ModelCall {
                            error: Some(err.to_string()),
                            ..call
                        },
                    }
                }
                Err(err) => ModelCall {
                    error: Some(format!("{err:#}")),
                    ..call
                },
            };
            stats.reads.push(call);
        }
        decided
    }
}

/// Applies reading decisions: picked blocks take the decided presentation or are left out, and
/// uncertain blocks that reading kept are added after the picks. Undecided picks stay as
/// screened, and undecided uncertain blocks are left out.
fn apply_reading(
    picks: &mut Vec<Pick>,
    uncertain: &[usize],
    decided: Vec<(usize, Option<Presentation>, Option<String>)>,
) {
    for (index, presentation, note) in decided {
        match (
            picks.iter().position(|pick| pick.index == index),
            presentation,
        ) {
            (Some(position), Some(presentation)) => picks[position].presentation = presentation,
            (Some(position), None) => {
                picks.remove(position);
            }
            (None, Some(presentation)) if uncertain.contains(&index) => picks.push(Pick {
                index,
                presentation,
                note,
            }),
            (None, _) => {}
        }
    }
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
