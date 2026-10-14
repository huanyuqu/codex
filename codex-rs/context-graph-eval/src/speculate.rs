//! Recall made ahead of the next turn, replayed from rollouts.
//!
//! When a turn ends, recall reads the turn's request and answer and chooses blocks for the next
//! turn, whose request is not known yet. The next turn starts from that recall, without
//! screening again, when it shows the blocks lexical search ranks highest for the request;
//! otherwise the turn waits briefly for the recall of its request and, if that takes longer,
//! goes on with the earlier recall and summaries of the best lexical matches. Replaying the turns
//! of a rollout measures how often the recall made ahead serves, how much of what the request's
//! own recall picks it shows, and how long turns would wait.

use std::collections::HashSet;

use codex_context_graph::BlockId;
use codex_context_graph::BlockKind;
use codex_context_graph::history::History;
use codex_context_graph::search::lexical_matches;
use codex_context_graph::text::item_text;
use codex_context_graph::view::Reason;
use codex_context_graph::view::ViewLimits;
use codex_context_graph_store::RecallStats;
use codex_context_graph_store::Recaller;
use codex_context_graph_store::speculative_question;
use serde::Deserialize;
use serde::Serialize;

/// Blocks lexical search ranks highest that a turn shows while the recall of its request runs.
const STAND_IN_MATCHES: usize = 3;

/// One replayed turn end and the request after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeculationResult {
    pub graph_id: String,
    /// The turn that ended.
    pub turn: usize,
    /// The next turn's request.
    pub question: String,
    /// Whether the recall made as the turn ended serves the request, so that the next turn
    /// starts from it without screening.
    pub served: bool,
    /// Blocks before the recent turns that the request's own recall shows for what it picked:
    /// the picks, what they depend on and what replaced them.
    pub needed_blocks: usize,
    /// Shares of those that the recall made ahead shows, that the best lexical matches show,
    /// and that either shows; unset when the request's recall picked nothing.
    pub speculative_coverage: Option<f64>,
    pub lexical_coverage: Option<f64>,
    pub combined_coverage: Option<f64>,
    pub speculative: RecallStats,
    pub request: RecallStats,
}

/// The turns of `history` to replay: those followed by a request that has history before the
/// `recent_turns` to recall from, at most `max`, spread evenly over the session.
pub fn replay_turns(history: &History<'_>, recent_turns: usize, max: usize) -> Vec<usize> {
    let turns: Vec<usize> = (1..history.last_turn())
        .filter(|turn| {
            // At the next request, the turns from `turn + 2 - recent_turns` on are recent.
            next_request_end(history, *turn).is_some_and(|end| {
                (0..end).any(|index| {
                    history.recallable(index) && history.turn(index) + recent_turns <= turn + 1
                })
            })
        })
        .collect();
    if turns.len() <= max {
        return turns;
    }
    (0..max)
        .map(|position| turns[position * turns.len() / max])
        .collect()
}

/// The number of blocks of `history` up to and including the request that starts the turn
/// after `turn`, if there is one.
pub fn next_request_end(history: &History<'_>, turn: usize) -> Option<usize> {
    (0..history.len())
        .find(|index| {
            history.turn(*index) == turn + 1
                && history.card(*index).kind == BlockKind::UserMessage
                && history.is_own(*index)
        })
        .map(|index| index + 1)
}

/// The text of the block at `index`.
fn block_text(history: &History<'_>, index: usize) -> String {
    history
        .block(index)
        .items
        .iter()
        .filter_map(item_text)
        .map(|text| text.text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The request that started `turn` and the turn's last answer.
fn request_and_answer(history: &History<'_>, turn: usize) -> (String, Option<String>) {
    let in_turn = |index: &usize| history.turn(*index) == turn && history.is_own(*index);
    let request = (0..history.len())
        .filter(in_turn)
        .find(|index| history.card(*index).kind == BlockKind::UserMessage)
        .map(|index| block_text(history, index))
        .unwrap_or_default();
    let answer = (0..history.len())
        .rev()
        .filter(in_turn)
        .find(|index| history.card(*index).kind == BlockKind::AssistantMessage)
        .map(|index| block_text(history, index));
    (request, answer)
}

/// Replays the end of `turn`, whose history is `ended`, and the request after it, whose
/// history up to that request is `next`. `speculative` recalls with one recent turn fewer than
/// `request`, since the next request makes the ended turn an earlier one.
pub async fn replay_turn(
    graph_id: &str,
    turn: usize,
    ended: &History<'_>,
    next: &History<'_>,
    speculative: &Recaller,
    request: &Recaller,
) -> anyhow::Result<SpeculationResult> {
    let (ended_request, answer) = request_and_answer(ended, turn);
    let ahead = speculative
        .select(
            ended,
            &speculative_question(&ended_request, answer.as_deref()),
        )
        .await?;
    let question = match next.len() {
        0 => String::new(),
        len => block_text(next, len - 1),
    };
    let own = request.select(next, &question).await?;

    let limits: ViewLimits = request.config().view;
    let served = ahead.serves(next, &question, limits, |_| false);
    let own_view = own.plan(next, limits).view;
    let needed: HashSet<&BlockId> = own_view
        .entries
        .iter()
        .filter(|entry| entry.index < own_view.recent_start)
        .filter(|entry| {
            matches!(
                entry.reason,
                Reason::Selected | Reason::Dependency | Reason::Successor
            )
        })
        .map(|entry| &entry.block_id)
        .collect();
    let ahead_view = ahead.plan(next, limits).view;
    let ahead_shown: HashSet<&BlockId> = ahead_view
        .entries
        .iter()
        .map(|entry| &entry.block_id)
        .collect();
    let stand_ins: HashSet<&BlockId> = lexical_matches(
        next,
        &question,
        next.recent_start(limits.recent_turns),
        STAND_IN_MATCHES,
    )
    .into_iter()
    .map(|index| &next.card(index).block_id)
    .collect();
    let coverage = |shown: &dyn Fn(&BlockId) -> bool| {
        (!needed.is_empty()).then(|| {
            needed.iter().filter(|block| shown(block)).count() as f64 / needed.len() as f64
        })
    };
    Ok(SpeculationResult {
        graph_id: graph_id.to_string(),
        turn,
        question,
        served,
        needed_blocks: needed.len(),
        speculative_coverage: coverage(&|block| ahead_shown.contains(block)),
        lexical_coverage: coverage(&|block| stand_ins.contains(block)),
        combined_coverage: coverage(&|block| {
            ahead_shown.contains(block) || stand_ins.contains(block)
        }),
        speculative: ahead.stats,
        request: own.stats,
    })
}

/// How long the turn after a replayed one would wait for recall, in milliseconds, when a turn
/// waits at most `wait_ms`: nothing when the recall made ahead serves it, else the recall of its
/// request, up to the wait.
pub fn waited_ms(result: &SpeculationResult, wait_ms: u64) -> u64 {
    if result.served {
        0
    } else {
        result.request.latency_ms.min(wait_ms)
    }
}

/// A summary of replayed turns, one line per measure.
pub fn speculation_report(results: &[SpeculationResult], wait_ms: u64) -> Vec<String> {
    if results.is_empty() {
        return vec!["No replayed turns.".to_string()];
    }
    let n = results.len();
    let share = |count: usize| format!("{:.1}% ({count} of {n})", count as f64 * 100.0 / n as f64);
    let served = results.iter().filter(|result| result.served).count();
    let deferred = results
        .iter()
        .filter(|result| !result.served && result.request.latency_ms > wait_ms)
        .count();
    let mut waits: Vec<u64> = results
        .iter()
        .map(|result| waited_ms(result, wait_ms))
        .collect();
    waits.sort_unstable();
    let mut request_latency: Vec<u64> = results
        .iter()
        .map(|result| result.request.latency_ms)
        .collect();
    request_latency.sort_unstable();
    let mean_coverage = |subset: &dyn Fn(&SpeculationResult) -> bool,
                         value: fn(&SpeculationResult) -> Option<f64>| {
        let values: Vec<f64> = results
            .iter()
            .filter(|result| subset(result))
            .filter_map(value)
            .collect();
        if values.is_empty() {
            "-".to_string()
        } else {
            format!(
                "{:.3} over {} turns",
                values.iter().sum::<f64>() / values.len() as f64,
                values.len()
            )
        }
    };
    let tokens = |stats: fn(&SpeculationResult) -> &RecallStats| {
        let (mut input, mut cached, mut output) = (0, 0, 0);
        for result in results {
            let stats = stats(result);
            for call in stats
                .screen
                .iter()
                .chain(&stats.reads)
                .chain(&stats.episode_screen)
                .chain(&stats.escalation)
            {
                input += call.input_tokens;
                cached += call.cached_input_tokens;
                output += call.output_tokens;
            }
        }
        format!("{input} input ({cached} cached), {output} output")
    };
    vec![
        format!("Replayed turns: {n}, waiting at most {wait_ms} ms for a request's recall."),
        format!(
            "Served by the recall made ahead, no screening: {}",
            share(served)
        ),
        format!(
            "Not served, the request's recall took longer than the wait: {}",
            share(deferred)
        ),
        format!(
            "Turn wait: mean {:.0} ms, p50 {} ms, p95 {} ms",
            waits.iter().sum::<u64>() as f64 / n as f64,
            percentile(&waits, 50),
            percentile(&waits, 95)
        ),
        format!(
            "Request recall latency: p50 {} ms, p95 {} ms",
            percentile(&request_latency, 50),
            percentile(&request_latency, 95)
        ),
        format!(
            "Share of the request's needed blocks the recall made ahead shows: served {}, not served {}",
            mean_coverage(&|result| result.served, |result| result
                .speculative_coverage),
            mean_coverage(&|result| !result.served, |result| result
                .speculative_coverage)
        ),
        format!(
            "Shown by the best lexical matches: {}; by either: {}",
            mean_coverage(&|_| true, |result| result.lexical_coverage),
            mean_coverage(&|_| true, |result| result.combined_coverage)
        ),
        format!(
            "Screening tokens: ahead {}; requests {}",
            tokens(|result| &result.speculative),
            tokens(|result| &result.request)
        ),
    ]
}

/// The `percent` percentile of sorted `values`.
fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let rank = (values.len() * percent).div_ceil(100).max(1);
    values[rank.min(values.len()) - 1]
}

#[cfg(test)]
#[path = "speculate_tests.rs"]
mod tests;
