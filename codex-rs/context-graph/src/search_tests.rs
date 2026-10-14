use pretty_assertions::assert_eq;

use super::*;
use crate::BlockKind;
use crate::test_support::card;
use crate::test_support::retry_session;

#[test]
fn terms_split_identifiers_paths_and_chinese() {
    assert_eq!(
        terms("SessionCardModel in core/src/context_graph.rs"),
        vec![
            "sessioncardmodel",
            "session",
            "card",
            "model",
            "in",
            "core",
            "src",
            "context_graph",
            "context",
            "graph",
            "rs",
        ]
    );
    assert_eq!(terms("修复测试"), vec!["修复", "复测", "测试"]);
    assert_eq!(terms("用 rg 查"), vec!["用", "rg", "查"]);
}

#[test]
fn cards_naming_the_query_rank_first() {
    let mut segmenter = card("b1", BlockKind::ToolExchange, None, "edit segmenter.rs");
    segmenter.entities.files_written = vec!["context-graph/src/segmenter.rs".to_string()];
    let mut tests = card("b2", BlockKind::ToolExchange, None, "$ cargo test");
    tests.summary = "the segmenter tests pass".to_string();
    let unrelated = card(
        "b3",
        BlockKind::UserMessage,
        None,
        "write the paper outline",
    );
    let cards = vec![&segmenter, &tests, &unrelated];

    let ranked = rank_cards(&cards, "why does segmenter.rs drop tool outputs?");
    let order: Vec<usize> = ranked.iter().map(|(index, _)| *index).collect();
    assert_eq!(order, vec![0, 1]);
}

#[test]
fn bm25_prefers_rare_terms() {
    let index = Bm25::new(["retry delay", "retry", "retry backoff delay"]);
    let scores = index.scores("backoff");
    assert!(scores[2] > 0.0);
    assert_eq!((scores[0], scores[1]), (0.0, 0.0));
}

#[test]
fn lexical_matches_rank_earlier_blocks_and_skip_the_recent_turns() {
    let session = retry_session();
    let history = session.history();
    let ids = |indices: Vec<usize>| -> Vec<String> {
        let mut ids: Vec<String> = indices
            .into_iter()
            .map(|index| history.card(index).block_id.to_string())
            .collect();
        ids.sort();
        ids
    };

    let recent_start = history.recent_start(1);
    assert_eq!(
        ids(lexical_matches(&history, "fixed delay", recent_start, 5)),
        vec!["a2", "u2"]
    );
    assert_eq!(
        lexical_matches(&history, "fixed delay", recent_start, 1).len(),
        1
    );
    // With turn 2 among the recent turns, nothing before them matches.
    assert_eq!(
        lexical_matches(&history, "fixed delay", history.recent_start(3), 5),
        Vec::<usize>::new()
    );
    assert_eq!(
        lexical_matches(&history, "", recent_start, 5),
        Vec::<usize>::new()
    );
}
