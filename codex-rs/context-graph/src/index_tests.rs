use pretty_assertions::assert_eq;

use super::*;
use crate::Branch;
use crate::DefaultClassifier;
use crate::test_support::assistant;
use crate::test_support::exec;
use crate::test_support::output_text;
use crate::test_support::rollout;
use crate::test_support::turn_complete;
use crate::test_support::user;

fn graph_with(branches: &[&str]) -> ContextGraph {
    let mut graph = ContextGraph::new();
    for branch in branches {
        graph
            .add_branch(Branch::new(BranchId::new(*branch), *branch))
            .unwrap_or_else(|err| panic!("add branch: {err}"));
    }
    graph
}

fn first_turn() -> Vec<RolloutItem> {
    vec![
        rollout(user("msg_u1", "run the tests")),
        rollout(exec(
            "fc_1",
            "call_1",
            r#"{"cmd":"cargo test","workdir":"/repo"}"#,
        )),
        rollout(output_text(
            "fco_1",
            "call_1",
            "Process exited with code 1\nFAILED: retry_test",
        )),
        rollout(assistant("msg_a1", "One test fails: retry_test.")),
        turn_complete("turn_1"),
    ]
}

fn index(graph: &mut ContextGraph, branch: &str, items: &[RolloutItem]) -> Vec<IndexUpdate> {
    let mut updates = Vec::new();
    index_rollout(
        graph,
        &BranchId::new(branch),
        items,
        DefaultClassifier,
        |update| updates.push(update),
    )
    .unwrap_or_else(|err| panic!("index: {err}"));
    updates
}

#[test]
fn every_new_block_gets_a_card_and_turns_get_turn_cards() {
    let mut graph = graph_with(&["br_main"]);
    let updates = index(&mut graph, "br_main", &first_turn());

    let cards: Vec<(String, String)> = updates
        .iter()
        .filter_map(|update| match update {
            IndexUpdate::Block { card, .. } => {
                Some((card.block_id.to_string(), card.title.clone()))
            }
            IndexUpdate::TurnEnded { .. } => None,
        })
        .collect();
    assert_eq!(
        cards,
        vec![
            ("blk_msg_u1".to_string(), "run the tests".to_string()),
            ("blk_fc_1".to_string(), "$ cargo test".to_string()),
            (
                "blk_msg_a1".to_string(),
                "One test fails: retry_test.".to_string()
            ),
        ]
    );

    let Some(IndexUpdate::TurnEnded { card: Some(turn) }) = updates.last() else {
        panic!("expected a turn card, got {updates:?}");
    };
    assert_eq!(turn.turn_id, "turn_1");
    assert_eq!(turn.request, "run the tests");
    assert_eq!(turn.result, "One test fails: retry_test.");
    assert_eq!((turn.commands, turn.failed_commands), (1, 1));
    assert_eq!(turn.blocks.len(), 3);
}

#[test]
fn inherited_history_gets_no_new_cards() {
    let mut graph = graph_with(&["br_main", "br_fork"]);
    index(&mut graph, "br_main", &first_turn());

    let mut fork_items = first_turn();
    fork_items.push(rollout(user("msg_u2", "try a different fix")));
    let updates = index(&mut graph, "br_fork", &fork_items);
    let new_cards: Vec<String> = updates
        .iter()
        .filter_map(|update| match update {
            IndexUpdate::Block { card, .. } => Some(card.block_id.to_string()),
            IndexUpdate::TurnEnded { .. } => None,
        })
        .collect();
    assert_eq!(new_cards, vec!["blk_msg_u2".to_string()]);
}
