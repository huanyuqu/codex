use pretty_assertions::assert_eq;

use super::*;
use crate::Branch;
use crate::DefaultClassifier;
use crate::replay_rollout;
use crate::test_support::assistant;
use crate::test_support::rollout;
use crate::test_support::user;

#[test]
fn renders_one_node_per_branch_and_an_edge_per_fork() {
    let shared = vec![
        rollout(user("msg_u1", "design the cache")),
        rollout(assistant("msg_a1", "two options")),
    ];
    let mut parent = shared.clone();
    parent.push(rollout(user("msg_u2", "take option A")));
    let mut child = shared;
    child.push(rollout(user("msg_f1", "explore option B")));

    let mut graph = ContextGraph::new();
    for name in ["main", "option-b"] {
        graph
            .add_branch(Branch::new(BranchId::new(name), name))
            .unwrap_or_else(|err| panic!("add branch: {err}"));
    }
    replay_rollout(
        &mut graph,
        &BranchId::new("main"),
        &parent,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay main: {err}"));
    replay_rollout(
        &mut graph,
        &BranchId::new("option-b"),
        &child,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay option-b: {err}"));

    assert_eq!(
        branch_mermaid(&graph),
        "flowchart LR\n    b0[\"main · 3 blocks\"]\n    b1[\"option-b · 1 blocks\"]\n    b0 -- \"after assistant_message\" --> b1\n"
    );
    assert_eq!(
        kind_counts(&graph),
        BTreeMap::from([
            ("assistant_message".to_string(), 1),
            ("user_message".to_string(), 3),
        ])
    );
}
