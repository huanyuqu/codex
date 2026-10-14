use pretty_assertions::assert_eq;

use super::*;
use crate::BlockId;
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

#[test]
fn answers_say_how_sure_the_screening_was() {
    let session = retry_session();
    let history = session.history();
    let prompt = build_screen_prompt(
        &history,
        "that bug from before",
        history.recent_start(1),
        ScreenLimits {
            max_uncertain: 2,
            ..ScreenLimits::default()
        },
    );
    let parse = |text: &str| {
        parse_screen_answer(&prompt, text).unwrap_or_else(|err| panic!("parse: {err}"))
    };
    let low = parse(r#"{"selected": [], "uncertain": [], "confidence": "low"}"#);
    assert_eq!(low.confidence, Confidence::Low);
    assert!(low.unsure(prompt.max_uncertain));

    let missing =
        parse(r#"{"selected": [{"card": "C8", "detail": "full", "reason": ""}], "uncertain": []}"#);
    assert_eq!(missing.confidence, Confidence::Medium);
    assert!(!missing.unsure(prompt.max_uncertain));

    // Leaving as many blocks undecided as allowed counts as unsure too.
    let saturated = parse(r#"{"selected": [], "uncertain": ["C2", "C3"], "confidence": "high"}"#);
    assert!(saturated.unsure(prompt.max_uncertain));
}

#[test]
fn episode_screening_narrows_the_catalog_to_the_chosen_episodes() {
    let session = retry_session();
    let episode = |first: &str, last: &str, title: &str| crate::episode::EpisodeCard {
        first_block: BlockId::from_stored(first),
        last_block: BlockId::from_stored(last),
        turns: 1,
        indexer: "test".to_string(),
        title: title.to_string(),
        summary: format!("summary of {title}"),
    };
    let history = session.history().with_episodes(&[
        episode("u1", "u2", "retry design"),
        episode("u3", "u3", "running the tests"),
    ]);
    let recent_start = history.recent_start(1);
    let prompt = build_episode_prompt(&history, "why did the test fail", recent_start)
        .unwrap_or_else(|| panic!("no episode prompt"));
    assert_eq!(prompt.episodes, vec![0, 1]);
    assert!(prompt.prompt.input.starts_with(
        "## Episodes\nE1 (turns 1-2) retry design — summary of retry design\n\
         E2 (turn 3) running the tests — summary of running the tests\n"
    ));

    let within = parse_episode_answer(&history, &prompt, r#"{"episodes": ["E2", "E9"]}"#)
        .unwrap_or_else(|err| panic!("parse: {err}"));
    // The chosen episode's blocks, every user message and the uncovered recent turn.
    for id in ["u1", "u2", "u3", "t3", "a3", "u4"] {
        assert!(within.contains(&session.index(id)), "{id} missing");
    }
    for id in ["r1", "a1", "a2", "e2"] {
        assert!(!within.contains(&session.index(id)), "{id} kept");
    }
    let narrowed = build_screen_prompt_within(
        &history,
        "why did the test fail",
        recent_start,
        ScreenLimits::default(),
        Some(&within),
    );
    assert!(
        narrowed
            .candidates
            .iter()
            .all(|index| within.contains(index))
    );

    assert!(build_episode_prompt(&session.history(), "q", recent_start).is_none());
}
