//! Recall: choosing, for one request, the blocks of a branch's history its model should see.
//!
//! A screening model reads the cards of the whole history and picks blocks, most important
//! first, with how much of each to show, and names the blocks its cards cannot decide. A history
//! too long for one catalog is first narrowed to the episodes the request needs. When the
//! screening is unsure, a stronger model screens again. A reading model then reads the undecided
//! blocks, and large picked blocks, in full, and decides for each whether to show it whole, an
//! excerpt or a summary, or to leave it out. Blocks that lexical search ranks high for the
//! request but no one picked are added as summaries, so a block the screening missed is not lost.
//! The picks become a [`ContextView`] that fits the token budget.
//!
//! Choosing ([`Recaller::select`]) is apart from planning the view ([`Selection::plan`]), and a
//! selection names blocks by ID: one made in the background, on a history that has grown since,
//! still plans a view of the history as it is now.
//!
//! Instead of one listwise screening, a [`RelevanceScorer`] can judge every card on its own
//! (pointwise), or narrow the candidates down for the listwise screening (cascade).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use codex_context_graph::BlockId;
use codex_context_graph::history::History;
use codex_context_graph::read::ReadLimits;
use codex_context_graph::read::build_read_prompts;
use codex_context_graph::read::parse_read_answer;
use codex_context_graph::score::candidate_documents;
use codex_context_graph::score::picks_from_scores;
use codex_context_graph::score::score_candidates;
use codex_context_graph::score::top_candidates;
use codex_context_graph::screen::ScreenAnswer;
use codex_context_graph::screen::ScreenLimits;
use codex_context_graph::screen::ScreenPrompt;
use codex_context_graph::screen::build_episode_prompt;
use codex_context_graph::screen::build_screen_prompt_within;
use codex_context_graph::screen::parse_episode_answer;
use codex_context_graph::screen::parse_screen_answer;
use codex_context_graph::search::lexical_matches;
use codex_context_graph::view::ContextView;
use codex_context_graph::view::Pick;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::Reason;
use codex_context_graph::view::ViewLimits;
use codex_context_graph::view::plan_view_with_matches;
use futures::future::join_all;
use serde::Deserialize;
use serde::Serialize;

use crate::model::ModelCompletion;
use crate::model::ShadowModel;
use crate::scorer::RelevanceScorer;

/// How recall screens the cards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenStrategy {
    /// One model reads the catalog of every card and chooses.
    #[default]
    Listwise,
    /// A scorer judges each card on its own.
    Pointwise,
    /// A scorer narrows the cards down, then a model chooses among the best.
    Cascade,
}

/// How recall screens, reads and budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallConfig {
    pub view: ViewLimits,
    pub screen: ScreenLimits,
    pub read: ReadLimits,
    /// Picked blocks shown in full that are larger than this are read, so that an excerpt may
    /// replace them.
    pub read_over_tokens: u32,
    pub strategy: ScreenStrategy,
    /// Cards a cascade keeps for the listwise screening.
    pub cascade_candidates: usize,
    /// Lexical matches a view may summarize besides the picks; 0 adds none.
    pub max_matches: usize,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            view: ViewLimits::default(),
            screen: ScreenLimits::default(),
            read: ReadLimits::default(),
            read_over_tokens: 3_000,
            strategy: ScreenStrategy::Listwise,
            cascade_candidates: 60,
            max_matches: 8,
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
    /// The screening that chose episodes before blocks, for histories too long for one
    /// catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_screen: Option<ModelCall>,
    /// The stronger model's screening, when the first one was unsure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<ModelCall>,
    /// The scorer that judged the cards, for pointwise and cascade screening.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scorer: Option<String>,
    /// Cards the scorer judged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub scored: usize,
    /// Lexical matches the view summarizes besides the picks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lexical: usize,
    /// How the view reached the context window, once it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<RecallDelivery>,
    /// How long the turn waited for the recall, in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero_ms")]
    pub waited_ms: u64,
}

/// How a recalled view reached the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecallDelivery {
    /// The turn waited for the recall of its request.
    Waited,
    /// The recall ran when the previous turn ended, before the request was known.
    Speculative,
    /// The recall was made for the previous turn's request.
    Earlier,
    /// The recall finished after its turn went on, and was added between model requests.
    Deferred,
    /// Lexical search alone chose, while the recall of the request ran.
    Lexical,
}

impl RecallDelivery {
    /// The name of the delivery, as records show it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Waited => "waited",
            Self::Speculative => "speculative",
            Self::Earlier => "earlier",
            Self::Deferred => "deferred",
            Self::Lexical => "lexical",
        }
    }
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes the field by reference"
)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes the field by reference"
)]
fn is_zero_ms(value: &u64) -> bool {
    *value == 0
}

/// The view a recall built, with its stats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recall {
    pub view: ContextView,
    pub stats: RecallStats,
}

/// A block a recall picked, with how much of it to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedBlock {
    pub block_id: BlockId,
    pub presentation: Presentation,
    /// Why it was picked, for the view record.
    pub note: Option<String>,
}

/// What a recall chose for a request, before it becomes a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub question: String,
    /// Picked blocks, most important first.
    pub picks: Vec<SelectedBlock>,
    /// Blocks that lexical search ranks high for the request and that no pick shows, best
    /// first.
    pub matches: Vec<BlockId>,
    pub stats: RecallStats,
}

impl Selection {
    /// The view of `history` that shows this selection. Blocks the history does not have are
    /// skipped, and blocks recorded since the selection was made are shown like any others, so
    /// a selection made on an earlier history still plans a view of the current one.
    pub fn plan(&self, history: &History<'_>, limits: ViewLimits) -> Recall {
        let picks: Vec<Pick> = self
            .picks
            .iter()
            .filter_map(|pick| {
                Some(Pick {
                    index: history.position(&pick.block_id)?,
                    presentation: pick.presentation.clone(),
                    note: pick.note.clone(),
                })
            })
            .collect();
        let matches: Vec<usize> = self
            .matches
            .iter()
            .filter_map(|block| history.position(block))
            .collect();
        let view = plan_view_with_matches(history, &picks, &matches, limits);
        let mut stats = self.stats.clone();
        stats.lexical = view
            .entries
            .iter()
            .filter(|entry| entry.reason == Reason::Lexical)
            .count();
        Recall { view, stats }
    }

    /// Whether the selection shows the block `block`, picked or as a lexical match.
    pub fn shows(&self, block: &BlockId) -> bool {
        self.picks.iter().any(|pick| &pick.block_id == block) || self.matches.contains(block)
    }
}

/// Screens and reads with the shadow agent's models.
pub struct Recaller {
    screen_model: Arc<dyn ShadowModel>,
    read_model: Option<Arc<dyn ShadowModel>>,
    escalate_model: Option<Arc<dyn ShadowModel>>,
    scorer: Option<Arc<dyn RelevanceScorer>>,
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
            escalate_model: None,
            scorer: None,
            config,
        }
    }

    /// Screens again with `model` when the first screening is unsure.
    pub fn with_escalation(mut self, model: Option<Arc<dyn ShadowModel>>) -> Self {
        self.escalate_model = model;
        self
    }

    /// Judges cards with `scorer` for pointwise and cascade screening; without one, those
    /// strategies screen listwise.
    pub fn with_scorer(mut self, scorer: Option<Arc<dyn RelevanceScorer>>) -> Self {
        self.scorer = scorer;
        self
    }

    pub fn config(&self) -> &RecallConfig {
        &self.config
    }

    /// Builds the view of `history` for `question`. Fails when screening fails; a failed
    /// reading leaves the screening picks as they were.
    pub async fn recall(&self, history: &History<'_>, question: &str) -> anyhow::Result<Recall> {
        let selection = self.select(history, question).await?;
        Ok(selection.plan(history, self.config.view))
    }

    /// Chooses the blocks of `history` that `question` needs. Fails when screening fails; a
    /// failed reading leaves the screening picks as they were.
    pub async fn select(&self, history: &History<'_>, question: &str) -> anyhow::Result<Selection> {
        let started = Instant::now();
        let recent_start = history.recent_start(self.config.view.recent_turns);
        let mut stats = RecallStats::default();
        let answer = match (self.config.strategy, &self.scorer) {
            (ScreenStrategy::Pointwise, Some(scorer)) => {
                let candidates = score_candidates(history, recent_start);
                let scores = self
                    .score(scorer.as_ref(), history, question, &candidates, &mut stats)
                    .await?;
                let (picks, uncertain) =
                    picks_from_scores(&candidates, &scores, self.config.screen.max_uncertain);
                ScreenAnswer {
                    picks,
                    uncertain,
                    confidence: Default::default(),
                }
            }
            (ScreenStrategy::Cascade, Some(scorer)) => {
                let candidates = score_candidates(history, recent_start);
                let scores = self
                    .score(scorer.as_ref(), history, question, &candidates, &mut stats)
                    .await?;
                let within: HashSet<usize> =
                    top_candidates(&candidates, &scores, self.config.cascade_candidates)
                        .into_iter()
                        .collect();
                self.screen(history, question, recent_start, Some(&within), &mut stats)
                    .await?
            }
            _ => {
                let within = self
                    .narrow_to_episodes(history, question, recent_start, &mut stats)
                    .await;
                self.screen(history, question, recent_start, within.as_ref(), &mut stats)
                    .await?
            }
        };
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
        let mut left_out = HashSet::new();
        match &self.read_model {
            Some(read_model) if !to_read.is_empty() => {
                stats.read = to_read.len();
                let decided = self
                    .read(read_model.as_ref(), history, question, &to_read, &mut stats)
                    .await;
                left_out = apply_reading(&mut picks, &answer.uncertain, decided);
            }
            // Without a reader, blocks screening could not decide are summarized.
            _ => picks.extend(answer.uncertain.iter().map(|index| Pick {
                index: *index,
                presentation: Presentation::Summary,
                note: Some("not read".to_string()),
            })),
        }

        // Blocks a reader left out after reading them are not brought back as matches.
        let matches = if self.config.max_matches == 0 {
            Vec::new()
        } else {
            lexical_matches(
                history,
                question,
                recent_start,
                self.config.max_matches + picks.len() + left_out.len(),
            )
            .into_iter()
            .filter(|index| {
                !left_out.contains(index) && !picks.iter().any(|pick| pick.index == *index)
            })
            .take(self.config.max_matches)
            .map(|index| history.card(index).block_id.clone())
            .collect()
        };
        stats.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(Selection {
            question: question.to_string(),
            picks: picks
                .into_iter()
                .map(|pick| SelectedBlock {
                    block_id: history.card(pick.index).block_id.clone(),
                    presentation: pick.presentation,
                    note: pick.note,
                })
                .collect(),
            matches,
            stats,
        })
    }

    /// Scores `candidates` with `scorer`, recording what it judged.
    async fn score(
        &self,
        scorer: &dyn RelevanceScorer,
        history: &History<'_>,
        question: &str,
        candidates: &[usize],
        stats: &mut RecallStats,
    ) -> anyhow::Result<Vec<f32>> {
        stats.scorer = Some(scorer.name().to_string());
        stats.scored = candidates.len();
        stats.candidates = candidates.len();
        let documents = candidate_documents(history, candidates);
        let scores = scorer.score(question, &documents).await?;
        anyhow::ensure!(
            scores.len() == candidates.len(),
            "the scorer answered for {} of {} cards",
            scores.len(),
            candidates.len()
        );
        Ok(scores)
    }

    /// The blocks of the episodes the request needs, when the whole history is too long for one
    /// catalog and has episodes; `None` to screen everything. A failed episode screening
    /// screens everything.
    async fn narrow_to_episodes(
        &self,
        history: &History<'_>,
        question: &str,
        recent_start: usize,
        stats: &mut RecallStats,
    ) -> Option<HashSet<usize>> {
        let whole = build_screen_prompt_within(
            history,
            question,
            recent_start,
            self.config.screen,
            /*within*/ None,
        );
        if !whole.prefiltered {
            return None;
        }
        let prompt = build_episode_prompt(history, question, recent_start)?;
        let started = Instant::now();
        let completion = self.screen_model.complete(&prompt.prompt).await;
        let call = ModelCall::new(self.screen_model.as_ref(), started);
        match completion.and_then(|completion| {
            let within = parse_episode_answer(history, &prompt, &completion.text)?;
            Ok((completion, within))
        }) {
            Ok((completion, within)) => {
                stats.episode_screen = Some(call.with_usage(&completion));
                Some(within)
            }
            Err(err) => {
                stats.episode_screen = Some(ModelCall {
                    error: Some(format!("{err:#}")),
                    ..call
                });
                None
            }
        }
    }

    /// Screens the catalog, limited to `within` when it is set, and screens again with the
    /// stronger model when the answer is unsure.
    async fn screen(
        &self,
        history: &History<'_>,
        question: &str,
        recent_start: usize,
        within: Option<&HashSet<usize>>,
        stats: &mut RecallStats,
    ) -> anyhow::Result<ScreenAnswer> {
        let prompt =
            build_screen_prompt_within(history, question, recent_start, self.config.screen, within);
        stats.candidates = prompt.candidates.len();
        stats.prefiltered = prompt.prefiltered || within.is_some();
        stats.catalog_tokens = prompt.catalog_tokens;
        let (answer, call) = self
            .screen_with(self.screen_model.as_ref(), &prompt)
            .await?;
        stats.screen = Some(call);
        let Some(escalate_model) = &self.escalate_model else {
            return Ok(answer);
        };
        if !answer.unsure(prompt.max_uncertain) {
            return Ok(answer);
        }
        // The stronger model reads the same prompt, so it can reuse the cached prefix.
        match self.screen_with(escalate_model.as_ref(), &prompt).await {
            Ok((escalated, call)) => {
                stats.escalation = Some(call);
                Ok(escalated)
            }
            Err(err) => {
                stats.escalation = Some(ModelCall {
                    error: Some(format!("{err:#}")),
                    ..ModelCall::new(escalate_model.as_ref(), Instant::now())
                });
                Ok(answer)
            }
        }
    }

    async fn screen_with(
        &self,
        model: &dyn ShadowModel,
        prompt: &ScreenPrompt,
    ) -> anyhow::Result<(ScreenAnswer, ModelCall)> {
        let started = Instant::now();
        let completion = model.complete(&prompt.prompt).await?;
        let call = ModelCall::new(model, started).with_usage(&completion);
        let answer = parse_screen_answer(prompt, &completion.text)?;
        Ok((answer, call))
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
/// screened, and undecided uncertain blocks are left out. Returns the blocks reading decided to
/// leave out.
fn apply_reading(
    picks: &mut Vec<Pick>,
    uncertain: &[usize],
    decided: Vec<(usize, Option<Presentation>, Option<String>)>,
) -> HashSet<usize> {
    let left_out = decided
        .iter()
        .filter(|(_, presentation, _)| presentation.is_none())
        .map(|(index, _, _)| *index)
        .collect();
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
    left_out
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod tests;
