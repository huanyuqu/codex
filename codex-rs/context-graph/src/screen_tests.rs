use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::retry_session;

#[test]
fn the_catalog_lists_every_recallable_block_by_turn_and_ends_with_the_request() {
    let session = retry_session();
    let history = session.history();
    let prompt = build_screen_prompt(
        &history,
        "Why did the test fail?",
        history.recent_start(1),
        ScreenLimits::default(),
    );

    assert_eq!(prompt.candidates.len(), history.len() - 1);
    assert_eq!(prompt.candidates[0], session.index("u1"));
    assert!(!prompt.prefiltered);
    let input = &prompt.prompt.input;
    assert!(input.starts_with(
        "## History\n### Turn 1\nC1 [user] add retries to the HTTP client — summary of add retries \
         to the HTTP client\n    - constraint: Do not change the public API\n\
         C2 [tool] $ cat src/client.rs — summary of $ cat src/client.rs | files: src/client.rs\n"
    ));
    assert!(input.contains("C8 [tool] $ cargo test (failed)"));
    assert!(input.ends_with(
        "## Context the agent already has\nTurn 4 is shown in full. The user's constraints and \
         decisions from every turn are listed.\n\n## Request\nWhy did the test fail?\n"
    ));
    assert_eq!(prompt.prompt.schema_name, "context_selection");
}

#[test]
fn long_histories_keep_user_messages_and_the_best_matches() {
    let session = retry_session();
    let history = session.history();
    let full = build_screen_prompt(&history, "q", history.len(), ScreenLimits::default());
    let user_tokens: u32 = ["u1", "u2", "u3", "u4"]
        .iter()
        .map(|id| {
            estimate_tokens(&card_lines(
                history.card(session.index(id)),
                ScreenLimits::default(),
            ))
        })
        .sum();
    let limits = ScreenLimits {
        max_catalog_tokens: user_tokens + 25,
        ..ScreenLimits::default()
    };
    assert!(full.catalog_tokens > limits.max_catalog_tokens);

    let prompt = build_screen_prompt(&history, "the cargo test failure", history.len(), limits);
    assert!(prompt.prefiltered);
    let kept: Vec<&str> = prompt
        .candidates
        .iter()
        .map(|index| history.card(*index).block_id.as_str())
        .collect();
    assert_eq!(kept, vec!["u1", "u2", "u3", "t3", "u4"]);
}

#[test]
fn answers_resolve_labels_and_drop_repeats_and_unknown_blocks() {
    let session = retry_session();
    let history = session.history();
    let prompt = build_screen_prompt(
        &history,
        "Why did the test fail?",
        history.recent_start(1),
        ScreenLimits {
            max_uncertain: 1,
            ..ScreenLimits::default()
        },
    );
    let answer = parse_screen_answer(
        &prompt,
        r#"```json
        {"selected": [
            {"card": "C8", "detail": "full", "reason": "the failing test output"},
            {"card": "c6", "detail": "summary", "reason": ""},
            {"card": "C8", "detail": "summary", "reason": "again"},
            {"card": "C99", "detail": "full", "reason": "unknown"}
        ],
        "uncertain": ["C8", "5", "C2"]}
        ```"#,
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));

    assert_eq!(
        answer.picks,
        vec![
            Pick {
                index: session.index("t3"),
                presentation: Presentation::Full,
                note: Some("the failing test output".to_string()),
            },
            Pick {
                index: session.index("e2"),
                presentation: Presentation::Summary,
                note: None,
            },
        ]
    );
    assert_eq!(answer.uncertain, vec![session.index("a2")]);
    assert!(parse_screen_answer(&prompt, "no json").is_err());
}
