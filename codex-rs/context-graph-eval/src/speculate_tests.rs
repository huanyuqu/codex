use std::sync::Arc;
use std::sync::Mutex;

use codex_context_graph::history::HistoryBlock;
use codex_context_graph::prompt::ModelPrompt;
use codex_context_graph_store::ModelCompletion;
use codex_context_graph_store::RecallConfig;
use codex_context_graph_store::ShadowModel;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::Session;

/// Picks the second card of every catalog, recording the inputs it read.
struct SecondCard {
    inputs: Mutex<Vec<String>>,
}

impl ShadowModel for SecondCard {
    fn name(&self) -> String {
        "second-card".to_string()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            if let Ok(mut inputs) = self.inputs.lock() {
                inputs.push(prompt.input.clone());
            }
            Ok(ModelCompletion {
                text: r#"{"selected": [{"card": "C2", "detail": "full", "reason": "the cache"}],
                    "uncertain": []}"#
                    .to_string(),
                input_tokens: 400,
                cached_input_tokens: 100,
                output_tokens: 20,
            })
        })
    }
}

fn prefix<'a>(session: &'a Session, end: usize) -> History<'a> {
    History::new(
        session
            .cards
            .iter()
            .zip(&session.items)
            .take(end)
            .map(|(card, items)| HistoryBlock {
                card,
                items,
                complete: true,
                origin: None,
                changed: false,
            })
            .collect(),
    )
}

fn recaller(model: Arc<SecondCard>, recent_turns: usize) -> Recaller {
    Recaller::new(
        model,
        /*read_model*/ None,
        RecallConfig {
            view: ViewLimits {
                recent_turns,
                ..ViewLimits::default()
            },
            ..RecallConfig::default()
        },
    )
}

#[test]
fn turns_are_replayed_when_the_next_request_has_earlier_history() {
    let session = Session::new();
    let history = session.history();
    // u2 starts the second turn, the third block.
    assert_eq!(next_request_end(&history, /*turn*/ 1), Some(3));
    assert_eq!(next_request_end(&history, /*turn*/ 3), None);
    assert_eq!(
        replay_turns(&history, /*recent_turns*/ 1, /*max*/ 8),
        vec![1, 2]
    );
    // With two recent turns, the first turn's request has nothing older to recall.
    assert_eq!(
        replay_turns(&history, /*recent_turns*/ 2, /*max*/ 8),
        vec![2]
    );
    assert_eq!(
        replay_turns(&history, /*recent_turns*/ 1, /*max*/ 1),
        vec![1]
    );
}

#[tokio::test]
async fn a_replayed_turn_compares_the_recall_made_ahead_with_the_requests_own() {
    let session = Session::new();
    let ended = prefix(&session, 4);
    let next = prefix(&session, 5);
    let model = Arc::new(SecondCard {
        inputs: Mutex::new(Vec::new()),
    });
    let result = replay_turn(
        "g",
        /*turn*/ 2,
        &ended,
        &next,
        &recaller(model.clone(), /*recent_turns*/ 0),
        &recaller(model.clone(), /*recent_turns*/ 1),
    )
    .await
    .unwrap_or_else(|err| panic!("replay: {err}"));

    assert_eq!(result.question, "Do the tests pass?");
    // Nothing earlier matches the request's words, so the recall made ahead serves it.
    assert!(result.served);
    // Both pick a1, which a2 replaced, so both show a1 and a2; the best lexical matches, on
    // "the", include a2.
    assert_eq!(result.needed_blocks, 2);
    assert_eq!(
        (
            result.speculative_coverage,
            result.lexical_coverage,
            result.combined_coverage
        ),
        (Some(1.0), Some(0.5), Some(1.0))
    );
    let inputs = model
        .inputs
        .lock()
        .map(|inputs| inputs.clone())
        .unwrap_or_default();
    // The recall made ahead reads the ended turn's request and answer.
    assert!(inputs[0].contains("Make the cache larger.\n\nRaised the cache to 512 entries."));
    assert!(inputs[1].contains("Do the tests pass?"));
}

#[test]
fn the_report_counts_served_and_deferred_turns_and_waits() {
    let result = |served: bool, latency_ms: u64| SpeculationResult {
        graph_id: "g".to_string(),
        turn: 1,
        question: "q".to_string(),
        served,
        needed_blocks: 2,
        speculative_coverage: Some(if served { 1.0 } else { 0.5 }),
        lexical_coverage: Some(0.5),
        combined_coverage: Some(1.0),
        speculative: RecallStats::default(),
        request: RecallStats {
            latency_ms,
            ..RecallStats::default()
        },
    };
    let results = [
        result(true, 5_000),
        result(false, 5_000),
        result(false, 1_000),
    ];
    assert_eq!(waited_ms(&results[0], 3_000), 0);
    assert_eq!(waited_ms(&results[1], 3_000), 3_000);
    assert_eq!(waited_ms(&results[2], 3_000), 1_000);

    let report = speculation_report(&results, 3_000).join("\n");
    for expected in [
        "Served by the recall made ahead, no screening: 33.3% (1 of 3)",
        "the request's recall took longer than the wait: 33.3% (1 of 3)",
        "Turn wait: mean 1333 ms, p50 1000 ms, p95 3000 ms",
        "served 1.000 over 1 turns, not served 0.500 over 2 turns",
    ] {
        assert!(
            report.contains(expected),
            "missing {expected:?} in:\n{report}"
        );
    }
    assert_eq!(speculation_report(&[], 3_000), vec!["No replayed turns."]);
}
