use pretty_assertions::assert_eq;

use crate::test_support::retry_session;
use crate::text::estimate_tokens;

#[test]
fn blocks_are_grouped_into_turns() {
    let session = retry_session();
    let history = session.history();
    let turns: Vec<usize> = (0..history.len())
        .map(|index| history.turn(index))
        .collect();
    assert_eq!(turns, vec![0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4]);
    assert_eq!(history.last_turn(), 4);
    assert_eq!(history.task_start(), Some(session.index("u1")));
    assert_eq!(history.position(&session.cards[3].block_id), Some(3));
}

#[test]
fn recent_turns_start_at_their_first_block() {
    let session = retry_session();
    let history = session.history();
    assert_eq!(history.recent_start(0), history.len());
    assert_eq!(history.recent_start(1), session.index("u4"));
    assert_eq!(history.recent_start(2), session.index("u3"));
    assert_eq!(history.recent_start(9), 0);
}

#[test]
fn harness_context_is_not_recallable_and_reasoning_is_not_counted() {
    let session = retry_session();
    let history = session.history();
    assert!(!history.recallable(session.index("ctx")));
    assert!(history.recallable(session.index("a1")));
    let a1 = session.index("a1");
    assert_eq!(
        history.tokens(a1),
        estimate_tokens("I will retry send() with exponential backoff.") + 8
    );
}

#[test]
fn token_estimates_count_chinese_characters() {
    assert_eq!(estimate_tokens("abcdefgh"), 2);
    assert_eq!(estimate_tokens("中文回答"), 4);
    assert_eq!(estimate_tokens("用中文 answer"), 3 + 2);
}

#[test]
fn referenced_blocks_keep_the_turns_and_are_never_recent() {
    let mut session = retry_session();
    let u4 = session.index("u4");
    session.insert_referenced(
        u4,
        "jitter",
        crate::test_support::card("j1", crate::BlockKind::UserMessage, None, "try jitter"),
        Vec::new(),
    );
    let history = session.history();
    let j1 = session.index("j1");
    assert_eq!(history.last_turn(), 4);
    assert_eq!(history.turn(j1), 4);
    assert_eq!(history.origin(j1), Some("jitter"));
    let recent_start = history.recent_start(1);
    assert_eq!(recent_start, u4);
    assert!(!history.is_recent(j1, recent_start));
    assert!(history.is_recent(u4, recent_start));
}

#[test]
fn episodes_map_to_turns_and_never_overlap() {
    use crate::episode::EpisodeCard;

    let session = retry_session();
    let episode = |first: &str, last: &str| EpisodeCard {
        first_block: crate::BlockId::from_stored(first),
        last_block: crate::BlockId::from_stored(last),
        turns: 0,
        indexer: "test".to_string(),
        title: format!("{first}-{last}"),
        summary: String::new(),
    };
    let history = session.history().with_episodes(&[
        episode("u3", "u4"),
        episode("u1", "u2"),
        // Overlaps the first, so it is dropped.
        episode("u2", "u3"),
        // Not in this history.
        episode("x1", "x2"),
    ]);
    let spans: Vec<(usize, usize)> = history
        .episodes()
        .iter()
        .map(|span| (span.first_turn, span.last_turn))
        .collect();
    assert_eq!(spans, vec![(1, 2), (3, 4)]);
}
