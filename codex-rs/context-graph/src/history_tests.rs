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
