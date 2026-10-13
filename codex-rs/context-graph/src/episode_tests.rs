use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::card;

fn turns(count: usize) -> Vec<TurnLine> {
    (1..=count)
        .map(|turn| TurnLine {
            start: BlockId::from_stored(format!("u{turn}")),
            request: format!("request {turn}"),
            result: format!("result {turn}"),
        })
        .collect()
}

#[test]
fn turns_start_at_user_messages_and_end_with_their_last_answer() {
    let cards = [
        card("ctx", BlockKind::ContextFragment, None, "harness"),
        card("u1", BlockKind::UserMessage, None, "add retries"),
        card("a1", BlockKind::AssistantMessage, None, "plan"),
        card("t1", BlockKind::ToolExchange, None, "edit client.rs"),
        card("a1b", BlockKind::AssistantMessage, None, "retries added"),
        card("u2", BlockKind::UserMessage, None, "run the tests"),
    ];
    assert_eq!(
        turn_lines(&cards),
        vec![
            TurnLine {
                start: BlockId::from_stored("u1"),
                request: "add retries".to_string(),
                result: "retries added".to_string(),
            },
            TurnLine {
                start: BlockId::from_stored("u2"),
                request: "run the tests".to_string(),
                result: String::new(),
            },
        ]
    );
}

#[test]
fn only_settled_turns_after_the_last_episode_are_consolidated() {
    let all = turns(20);
    // Too few turns: 7 turns, the latest 2 unsettled, leave 5.
    assert!(unsettled_turns(&all[..7], &[]).is_empty());
    assert_eq!(unsettled_turns(&all[..8], &[]).len(), 6);

    let episodes = extracted_episodes(&all[..8]);
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].last_block, BlockId::from_stored("u8"));
    // Turns 9 to 18 are settled and after the episode.
    let rest = unsettled_turns(&all, &episodes);
    assert_eq!(rest.first().map(|turn| turn.start.as_str()), Some("u9"));
    assert_eq!(rest.len(), 10);
}

#[test]
fn extracted_episodes_chunk_the_turns() {
    let episodes = extracted_episodes(&turns(10));
    let spans: Vec<(&str, &str, usize)> = episodes
        .iter()
        .map(|episode| {
            (
                episode.first_block.as_str(),
                episode.last_block.as_str(),
                episode.turns,
            )
        })
        .collect();
    assert_eq!(spans, vec![("u1", "u8", 8), ("u9", "u10", 2)]);
    assert_eq!(episodes[0].title, "request 1");
    assert_eq!(
        episodes[0].summary,
        "Started with: request 1. Ended with: result 8"
    );
}

#[test]
fn model_episodes_must_cover_every_turn_in_order() {
    let all = turns(5);
    let parsed = parse_episodes(
        r#"{"episodes":[
            {"first_turn":1,"last_turn":3,"title":"retries","summary":"added retries"},
            {"first_turn":4,"last_turn":5,"title":"docs","summary":"wrote docs"}]}"#,
        &all,
        "luna",
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].first_block, BlockId::from_stored("u4"));
    assert_eq!(parsed[1].indexer, "luna");

    for bad in [
        // A gap.
        r#"{"episodes":[{"first_turn":1,"last_turn":2,"title":"a","summary":""},
            {"first_turn":4,"last_turn":5,"title":"b","summary":""}]}"#,
        // Not every turn.
        r#"{"episodes":[{"first_turn":1,"last_turn":4,"title":"a","summary":""}]}"#,
        // Past the end.
        r#"{"episodes":[{"first_turn":1,"last_turn":6,"title":"a","summary":""}]}"#,
    ] {
        assert!(parse_episodes(bad, &all, "luna").is_err(), "{bad}");
    }
}

#[test]
fn branch_cards_come_from_models_or_from_the_turns() {
    let all = turns(3);
    assert_eq!(
        extracted_branch_card(&all, 7),
        Some(BranchCard {
            indexer: EXTRACTED_INDEXER.to_string(),
            title: "request 1".to_string(),
            summary: "Latest: result 3".to_string(),
            turns: 7,
        })
    );
    assert_eq!(extracted_branch_card(&[], 0), None);
    let card = parse_branch_card(
        r#"{"title":"Retries","summary":"Retries done; docs left."}"#,
        "luna",
        3,
    )
    .unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(card.title, "Retries");
    assert!(parse_branch_card(r#"{"title":" ","summary":""}"#, "luna", 3).is_err());
    let prompt = branch_prompt("main", &extracted_episodes(&turns(8)), &all);
    assert!(prompt.input.contains("## Branch \"main\""));
    assert!(prompt.input.contains("1. request 1 → result 1"));
}
