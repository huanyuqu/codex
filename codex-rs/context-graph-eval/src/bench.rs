//! Asks probes under different contexts and grades the answers against the references.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use codex_context_graph::BlockId;
use codex_context_graph::history::History;
use codex_context_graph::prompt::ModelPrompt;
use codex_context_graph::prompt::parse_answer;
use codex_context_graph::search::rank_cards;
use codex_context_graph::text::estimate_tokens;
use codex_context_graph::view::ContextView;
use codex_context_graph::view::Pick;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::Reason;
use codex_context_graph::view::ViewEntry;
use codex_context_graph::view::entry_text;
use codex_context_graph::view::plan_view;
use codex_context_graph::view::render_transcript;
use codex_context_graph_store::ModelCall;
use codex_context_graph_store::ModelCompletion;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::RecallStats;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::ShadowModel;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use crate::probe::Probe;

const ANSWER_INSTRUCTIONS: &str = include_str!("../prompts/answer.md");
const JUDGE_INSTRUCTIONS: &str = include_str!("../prompts/judge.md");

/// What context a probe is asked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// Every block of the history; past `max_full_tokens`, the oldest blocks after the first
    /// request are dropped.
    Full,
    /// Only the most recent turns.
    Window,
    /// Blocks ranked by BM25 against the question, within the view budget.
    Bm25,
    /// Recall with a screening model and, unless `read` is `None`, a reading model.
    Recall {
        screen: String,
        read: Option<String>,
    },
    /// The probe's support blocks, as an upper bound for selection.
    Oracle,
}

impl FromStr for Condition {
    type Err = anyhow::Error;

    /// Parses `full`, `window`, `bm25`, `oracle`, `recall:SCREEN` (reads with the screening
    /// model), `recall:SCREEN+READ` or `recall:SCREEN+none`.
    fn from_str(name: &str) -> anyhow::Result<Self> {
        Ok(match name {
            "full" => Self::Full,
            "window" => Self::Window,
            "bm25" => Self::Bm25,
            "oracle" => Self::Oracle,
            _ => {
                let models = name
                    .strip_prefix("recall:")
                    .with_context(|| format!("unknown condition {name}"))?;
                let (screen, read) = match models.split_once('+') {
                    Some((screen, "none")) => (screen, None),
                    Some((screen, read)) => (screen, Some(read.to_string())),
                    None => (models, Some(models.to_string())),
                };
                Self::Recall {
                    screen: screen.to_string(),
                    read,
                }
            }
        })
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::Window => write!(f, "window"),
            Self::Bm25 => write!(f, "bm25"),
            Self::Oracle => write!(f, "oracle"),
            Self::Recall { screen, read } => match read {
                Some(read) if read == screen => write!(f, "recall:{screen}"),
                Some(read) => write!(f, "recall:{screen}+{read}"),
                None => write!(f, "recall:{screen}+none"),
            },
        }
    }
}

/// The context a probe is asked with.
pub struct ProbeContext {
    pub transcript: String,
    pub tokens: u32,
    /// Blocks the context shows, with how much of each.
    pub shown: Vec<(BlockId, Presentation, bool)>,
    pub recall: Option<RecallStats>,
    /// Whether `Full` had to drop blocks.
    pub truncated: bool,
}

/// Builds probe contexts.
pub struct ContextBuilder {
    pub config: RecallConfig,
    pub max_full_tokens: u32,
}

impl ContextBuilder {
    pub async fn build(
        &self,
        condition: &Condition,
        history: &History<'_>,
        probe: &Probe,
        recaller: Option<&Recaller>,
    ) -> anyhow::Result<ProbeContext> {
        let view = match condition {
            Condition::Full => return Ok(self.full(history)),
            Condition::Window => {
                let start = history.recent_start(self.config.view.recent_turns);
                return Ok(plain(history, start..history.len()));
            }
            Condition::Bm25 => {
                let recent_start = history.recent_start(self.config.view.recent_turns);
                let earlier: Vec<usize> = (0..recent_start)
                    .filter(|index| history.recallable(*index))
                    .collect();
                let cards: Vec<_> = earlier.iter().map(|index| history.card(*index)).collect();
                let picks: Vec<Pick> = rank_cards(&cards, &probe.question)
                    .into_iter()
                    .map(|(position, _)| Pick {
                        index: earlier[position],
                        presentation: Presentation::Full,
                        note: None,
                    })
                    .collect();
                plan_view(history, &picks, self.config.view)
            }
            Condition::Oracle => {
                let picks: Vec<Pick> = probe
                    .support
                    .iter()
                    .filter_map(|id| history.position(id))
                    .map(|index| Pick {
                        index,
                        presentation: Presentation::Full,
                        note: None,
                    })
                    .collect();
                plan_view(history, &picks, self.config.view)
            }
            Condition::Recall { .. } => {
                let recaller = recaller.context("recall needs a recaller")?;
                let recall = recaller.recall(history, &probe.question).await?;
                let mut context = from_view(history, &recall.view);
                context.recall = Some(recall.stats);
                return Ok(context);
            }
        };
        Ok(from_view(history, &view))
    }

    /// Every recallable block; past the cap, the oldest after the first request are dropped.
    fn full(&self, history: &History<'_>) -> ProbeContext {
        let task = history.task_start();
        let mut kept: Vec<usize> = (0..history.len())
            .filter(|index| history.recallable(*index))
            .collect();
        let mut tokens: u32 = kept.iter().map(|index| history.tokens(*index)).sum();
        let mut truncated = false;
        while tokens > self.max_full_tokens
            && let Some(position) = kept.iter().position(|index| Some(*index) != task)
        {
            tokens -= history.tokens(kept.remove(position));
            truncated = true;
        }
        let mut context = plain(history, kept);
        context.truncated = truncated;
        context
    }
}

/// The recallable blocks at `indices`, shown in full without the recall preamble.
fn plain(history: &History<'_>, indices: impl IntoIterator<Item = usize>) -> ProbeContext {
    let entries: Vec<ViewEntry> = indices
        .into_iter()
        .filter(|index| history.recallable(*index))
        .map(|index| ViewEntry {
            block_id: history.card(index).block_id.clone(),
            index,
            presentation: Presentation::Full,
            reason: Reason::Recent,
            note: None,
            superseded_by: None,
            file_changed: false,
            tokens: history.tokens(index),
        })
        .collect();
    let transcript = entries
        .iter()
        .map(|entry| entry_text(history, entry))
        .collect::<Vec<_>>()
        .join("\n\n");
    ProbeContext {
        tokens: estimate_tokens(&transcript),
        transcript,
        shown: entries
            .into_iter()
            .map(|entry| (entry.block_id, entry.presentation, false))
            .collect(),
        recall: None,
        truncated: false,
    }
}

fn from_view(history: &History<'_>, view: &ContextView) -> ProbeContext {
    let transcript = render_transcript(history, view);
    ProbeContext {
        tokens: estimate_tokens(&transcript),
        transcript,
        shown: view
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.block_id.clone(),
                    entry.presentation.clone(),
                    entry.superseded_by.is_some(),
                )
            })
            .collect(),
        recall: None,
        truncated: false,
    }
}

/// The answer to a probe under one condition, graded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub probe_id: String,
    pub category: String,
    pub condition: String,
    pub answer: String,
    pub verdict: String,
    #[serde(default)]
    pub missed: Vec<String>,
    #[serde(default)]
    pub judge_reason: String,
    pub context_tokens: u32,
    #[serde(default)]
    pub truncated: bool,
    pub shown_blocks: usize,
    /// Share of the support blocks the context shows at all.
    pub support_shown: f64,
    /// Share of the support blocks the context shows in full or as an excerpt.
    pub support_detailed: f64,
    /// Outdated blocks the context shows without marking them outdated.
    pub outdated_unmarked: usize,
    pub answer_call: ModelCall,
    pub judge_call: ModelCall,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<RecallStats>,
}

impl ProbeResult {
    /// 1 for correct, 0.5 for partial, 0 otherwise.
    pub fn score(&self) -> f64 {
        match self.verdict.as_str() {
            "correct" => 1.0,
            "partial" => 0.5,
            _ => 0.0,
        }
    }
}

/// Asks `probe` with `context` and grades the answer.
pub async fn answer_and_judge(
    probe: &Probe,
    condition: &Condition,
    context: ProbeContext,
    answer_model: &Arc<dyn ShadowModel>,
    judge_model: &Arc<dyn ShadowModel>,
) -> anyhow::Result<ProbeResult> {
    let (answer, answer_call) = complete::<Answer>(answer_model, answer_prompt(&context, probe))
        .await
        .context("answering failed")?;
    let (judgment, judge_call) =
        complete::<Judgment>(judge_model, judge_prompt(probe, &answer.answer))
            .await
            .context("judging failed")?;

    let shown: HashSet<&BlockId> = context.shown.iter().map(|(id, _, _)| id).collect();
    let detailed: HashSet<&BlockId> = context
        .shown
        .iter()
        .filter(|(_, presentation, _)| *presentation != Presentation::Summary)
        .map(|(id, _, _)| id)
        .collect();
    let share = |set: &HashSet<&BlockId>| {
        probe.support.iter().filter(|id| set.contains(id)).count() as f64
            / probe.support.len().max(1) as f64
    };
    let outdated_unmarked = context
        .shown
        .iter()
        .filter(|(id, _, marked)| !marked && probe.outdated.contains(id))
        .count();
    Ok(ProbeResult {
        probe_id: probe.id.clone(),
        category: probe.category.clone(),
        condition: condition.to_string(),
        answer: answer.answer,
        verdict: judgment.verdict,
        missed: judgment.missed,
        judge_reason: judgment.reason,
        context_tokens: context.tokens,
        truncated: context.truncated,
        shown_blocks: context.shown.len(),
        support_shown: share(&shown),
        support_detailed: share(&detailed),
        outdated_unmarked,
        answer_call,
        judge_call,
        recall: context.recall,
    })
}

#[derive(Deserialize)]
struct Answer {
    answer: String,
}

#[derive(Deserialize)]
struct Judgment {
    verdict: String,
    #[serde(default)]
    missed: Vec<String>,
    #[serde(default)]
    reason: String,
}

fn answer_prompt(context: &ProbeContext, probe: &Probe) -> ModelPrompt {
    ModelPrompt {
        instructions: ANSWER_INSTRUCTIONS.to_string(),
        input: format!(
            "## Session history\n\n{}\n\n## The user's newest message\n\n{}\n",
            context.transcript, probe.question
        ),
        schema_name: "answer",
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["answer"],
            "properties": { "answer": { "type": "string" } }
        }),
    }
}

fn judge_prompt(probe: &Probe, answer: &str) -> ModelPrompt {
    let facts: String = probe
        .key_facts
        .iter()
        .map(|fact| format!("- {fact}\n"))
        .collect();
    ModelPrompt {
        instructions: JUDGE_INSTRUCTIONS.to_string(),
        input: format!(
            "## Question\n{}\n\n## Reference answer\n{}\n\n## Key facts\n{facts}\n## Answer to grade\n{answer}\n",
            probe.question, probe.answer
        ),
        schema_name: "judgment",
        schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["verdict", "missed", "reason"],
            "properties": {
                "verdict": { "type": "string", "enum": ["correct", "partial", "incorrect"] },
                "missed": { "type": "array", "items": { "type": "string" } },
                "reason": { "type": "string" }
            }
        }),
    }
}

async fn complete<T: serde::de::DeserializeOwned>(
    model: &Arc<dyn ShadowModel>,
    prompt: ModelPrompt,
) -> anyhow::Result<(T, ModelCall)> {
    let started = Instant::now();
    let completion: ModelCompletion = model.complete(&prompt).await?;
    let call = ModelCall {
        model: model.name().to_string(),
        input_tokens: completion.input_tokens,
        cached_input_tokens: completion.cached_input_tokens,
        output_tokens: completion.output_tokens,
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        error: None,
    };
    Ok((parse_answer(&completion.text)?, call))
}

#[cfg(test)]
#[path = "bench_tests.rs"]
mod tests;
