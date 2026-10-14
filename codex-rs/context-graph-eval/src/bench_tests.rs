use std::path::PathBuf;
use std::sync::Mutex;

use codex_context_graph::view::ViewLimits;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::Session;

fn probe() -> Probe {
    Probe {
        id: "g#3#0".to_string(),
        rollout: PathBuf::from("rollout.jsonl"),
        graph_id: "g".to_string(),
        turn: 3,
        category: "superseded".to_string(),
        question: "How large is the cache now?".to_string(),
        answer: "512 entries".to_string(),
        key_facts: vec!["The cache holds 512 entries".to_string()],
        support: vec![BlockId::from_stored("a2")],
        outdated: vec![BlockId::from_stored("a1")],
        generator: "model".to_string(),
    }
}

fn builder(max_full_tokens: u32) -> ContextBuilder {
    ContextBuilder {
        config: RecallConfig {
            view: ViewLimits {
                recent_turns: 1,
                ..ViewLimits::default()
            },
            ..RecallConfig::default()
        },
        max_full_tokens,
    }
}

fn shown(context: &ProbeContext) -> Vec<&str> {
    context
        .shown
        .iter()
        .map(|block| block.block_id.as_str())
        .collect()
}

#[test]
fn conditions_parse_and_print_their_names() {
    for name in [
        "full",
        "window",
        "bm25",
        "oracle",
        "recall:luna",
        "recall:luna+terra",
        "recall:luna+none",
        "recall:luna+none@cascade",
        "recall:luna@pointwise@lex0",
        "recall:luna@lex25",
    ] {
        let condition: Condition = name.parse().unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(condition.to_string(), name);
    }
    assert_eq!(
        "recall:luna+terra".parse::<Condition>().ok(),
        Some(Condition::Recall {
            screen: "luna".to_string(),
            read: Some("terra".to_string()),
            strategy: ScreenStrategy::Listwise,
            lexical_share: None,
        })
    );
    assert_eq!(
        "recall:luna+none@cascade@lex0".parse::<Condition>().ok(),
        Some(Condition::Recall {
            screen: "luna".to_string(),
            read: None,
            strategy: ScreenStrategy::Cascade,
            lexical_share: Some(0),
        })
    );
    // The listwise default is not spelled out.
    assert_eq!(
        "recall:luna@listwise"
            .parse::<Condition>()
            .map(|condition| condition.to_string())
            .ok(),
        Some("recall:luna".to_string())
    );
    for bad in [
        "everything",
        "recall:",
        "recall:luna@lex60",
        "recall:luna@fast",
    ] {
        assert!(bad.parse::<Condition>().is_err(), "{bad}");
    }
}

#[tokio::test]
async fn contexts_show_what_their_condition_selects() {
    let session = Session::new();
    let history = session.history();
    let probe = probe();
    let build = |condition: Condition, max_full_tokens| {
        let history = &history;
        let probe = &probe;
        async move {
            builder(max_full_tokens)
                .build(&condition, history, probe, None)
                .await
                .unwrap_or_else(|err| panic!("{condition}: {err}"))
        }
    };

    let full = build(Condition::Full, 10_000).await;
    assert_eq!(shown(&full), vec!["u1", "a1", "u2", "a2", "u3", "a3"]);
    assert!(!full.truncated);

    // Past the cap, the oldest blocks after the first request go first.
    let capped = build(Condition::Full, 45).await;
    assert_eq!(shown(&capped), vec!["u1", "u3", "a3"]);
    assert!(capped.truncated);

    let window = build(Condition::Window, 10_000).await;
    assert_eq!(shown(&window), vec!["u3", "a3"]);

    let oracle = build(Condition::Oracle, 10_000).await;
    assert_eq!(shown(&oracle), vec!["u1", "a2", "u3", "a3"]);
    assert!(
        oracle
            .transcript
            .starts_with("Parts of this conversation's history")
    );

    let bm25 = build(Condition::Bm25, 10_000).await;
    assert_eq!(shown(&bm25), vec!["u1", "a1", "u2", "a2", "u3", "a3"]);
    // The outdated block is shown, but marked as replaced.
    assert!(bm25.shown[1].marked_outdated);
}

/// Answers with a fixed text and judges every answer correct, recording the inputs.
struct FixedModel {
    inputs: Mutex<Vec<String>>,
}

impl ShadowModel for FixedModel {
    fn name(&self) -> &str {
        "fixed"
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            if let Ok(mut inputs) = self.inputs.lock() {
                inputs.push(prompt.input.clone());
            }
            let text = match prompt.schema_name {
                "answer" => r#"{"answer": "512 entries"}"#,
                _ => r#"{"verdict": "correct", "missed": [], "reason": "states the size"}"#,
            };
            Ok(ModelCompletion {
                text: text.to_string(),
                input_tokens: 300,
                cached_input_tokens: 0,
                output_tokens: 10,
            })
        })
    }
}

#[tokio::test]
async fn answers_are_judged_and_selection_is_measured() {
    let session = Session::new();
    let history = session.history();
    let probe = probe();
    let context = builder(10_000)
        .build(&Condition::Window, &history, &probe, None)
        .await
        .unwrap_or_else(|err| panic!("context: {err}"));
    let model = Arc::new(FixedModel {
        inputs: Mutex::new(Vec::new()),
    });
    let shadow: Arc<dyn ShadowModel> = model.clone();
    let result = answer_and_judge(&probe, &Condition::Window, context, &shadow, &shadow)
        .await
        .unwrap_or_else(|err| panic!("answer: {err}"));

    assert_eq!(
        (
            result.verdict.as_str(),
            result.score(),
            result.support_shown,
            result.shown_blocks,
            result.outdated_unmarked,
            result.selected_blocks,
            result.selection_precision,
        ),
        ("correct", 1.0, 0.0, 2, 0, 0, None)
    );
    let inputs = model
        .inputs
        .lock()
        .map(|inputs| inputs.clone())
        .unwrap_or_default();
    assert!(inputs[0].ends_with("## The user's newest message\n\nHow large is the cache now?\n"));
    assert!(inputs[1].contains("## Key facts\n- The cache holds 512 entries\n"));
}

#[tokio::test]
async fn selection_precision_counts_the_earlier_blocks_shown() {
    let session = Session::new();
    let history = session.history();
    let probe = probe();
    let model: Arc<dyn ShadowModel> = Arc::new(FixedModel {
        inputs: Mutex::new(Vec::new()),
    });
    let measure = |condition: Condition| {
        let history = &history;
        let probe = &probe;
        let model = &model;
        async move {
            let context = builder(10_000)
                .build(&condition, history, probe, None)
                .await
                .unwrap_or_else(|err| panic!("{condition}: {err}"));
            let result = answer_and_judge(probe, &condition, context, model, model)
                .await
                .unwrap_or_else(|err| panic!("{condition}: {err}"));
            (result.selected_blocks, result.selection_precision)
        }
    };

    // The oracle shows the first request and the support block before the recent turn.
    assert_eq!(measure(Condition::Oracle).await, (2, Some(0.5)));
    // The full history shows all four earlier blocks, one of them support.
    assert_eq!(measure(Condition::Full).await, (4, Some(0.25)));
}
