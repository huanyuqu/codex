use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::retry_session;

#[test]
fn search_lists_matching_blocks_with_their_ids() {
    let session = retry_session();
    let history = session.history();
    assert_eq!(
        search_history(&history, "cargo test", 1),
        "t3 (turn 3, tool_exchange): $ cargo test — summary of $ cargo test"
    );
    assert_eq!(
        search_history(&history, "kubernetes", 5),
        "No earlier block matches \"kubernetes\"."
    );
}

#[test]
fn blocks_and_turns_expand_to_their_text() {
    let session = retry_session();
    let history = session.history();
    assert_eq!(
        expand_block(&history, "a2").as_deref(),
        Some(
            "## a2 (turn 2, assistant_message): switch to a fixed delay\n[assistant]\nSwitched to \
             a fixed 100 ms delay."
        )
    );
    assert_eq!(expand_block(&history, "nope"), None);

    let turn = expand_turn(&history, 2, 10_000).unwrap_or_default();
    assert!(turn.starts_with("## u2 (turn 2, user_message): use a fixed delay instead\n"));
    assert!(turn.contains("## e2 (turn 2, tool_exchange): edit client.rs\n"));
    assert_eq!(expand_turn(&history, 9, 10_000), None);
}
