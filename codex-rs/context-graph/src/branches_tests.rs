use pretty_assertions::assert_eq;

use super::*;
use crate::Block;
use crate::test_support::card;
use crate::test_support::claim;

fn block(id: &str, kind: BlockKind) -> Block {
    Block {
        id: BlockId::from_stored(id),
        kind,
        item_keys: vec![id.to_string()],
        turn_id: None,
        first_ordinal: 0,
        last_ordinal: 0,
        approx_bytes: 0,
        complete: true,
    }
}

/// `main` asks for retries (`u1`, `a1`) and later for a timeout (`u3`, `a3`); `jitter` forks
/// after `a1`, tries jitter (`u2`, `t2`, `a2`) and decides on it.
fn forked_graph() -> (ContextGraph, HashMap<BlockId, BlockCard>) {
    let mut graph = ContextGraph::new();
    let main = BranchId::new("main");
    let jitter = BranchId::new("jitter");
    let mut cards = HashMap::new();
    let mut append = |graph: &mut ContextGraph, branch: &BranchId, id: &str, kind, title: &str| {
        graph
            .append(branch, block(id, kind))
            .unwrap_or_else(|err| panic!("append {id}: {err}"));
        cards.insert(BlockId::from_stored(id), card(id, kind, None, title));
    };
    graph
        .add_branch(Branch::new(main.clone(), "main"))
        .unwrap_or_else(|err| panic!("add main: {err}"));
    append(
        &mut graph,
        &main,
        "u1",
        BlockKind::UserMessage,
        "add retries",
    );
    append(
        &mut graph,
        &main,
        "a1",
        BlockKind::AssistantMessage,
        "retries added",
    );
    let mut fork = Branch::new(jitter.clone(), "jitter");
    fork.parent = Some(main.clone());
    fork.fork_point = Some(BlockId::from_stored("a1"));
    fork.head = Some(BlockId::from_stored("a1"));
    fork.thread_id = Some("thread-jitter".to_string());
    graph
        .add_branch(fork)
        .unwrap_or_else(|err| panic!("add fork: {err}"));
    append(
        &mut graph,
        &jitter,
        "u2",
        BlockKind::UserMessage,
        "try jitter",
    );
    append(
        &mut graph,
        &jitter,
        "t2",
        BlockKind::ToolExchange,
        "$ cargo test jitter",
    );
    append(
        &mut graph,
        &jitter,
        "a2",
        BlockKind::AssistantMessage,
        "jitter works",
    );
    append(
        &mut graph,
        &main,
        "u3",
        BlockKind::UserMessage,
        "add a timeout",
    );
    append(
        &mut graph,
        &main,
        "a3",
        BlockKind::AssistantMessage,
        "timeout added",
    );
    if let Some(a2) = cards.get_mut(&BlockId::from_stored("a2")) {
        a2.claims = vec![
            claim(ClaimKind::Decision, "Use full jitter on the backoff"),
            claim(ClaimKind::Todo, "Tune the cap"),
        ];
    }
    (graph, cards)
}

#[test]
fn branches_are_listed_as_a_tree() {
    let (graph, cards) = forked_graph();
    let rows = branch_rows(&graph, &cards, &BranchId::new("jitter"));
    let summary: Vec<(&str, usize, usize, Option<&str>, bool)> = rows
        .iter()
        .map(|row| {
            (
                row.name.as_str(),
                row.depth,
                row.turns,
                row.last_request.as_deref(),
                row.current,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("main", 0, 2, Some("add a timeout"), false),
            ("jitter", 1, 1, Some("try jitter"), true),
        ]
    );
    assert_eq!(rows[1].thread_id.as_deref(), Some("thread-jitter"));
}

#[test]
fn folding_summarizes_what_the_branch_did_after_it_diverged() {
    let (graph, cards) = forked_graph();
    let fold = fold_summary(
        &graph,
        &cards,
        &BranchId::new("jitter"),
        &BranchId::new("main"),
        Since::Unmerged,
    )
    .unwrap_or_else(|err| panic!("fold: {err}"))
    .unwrap_or_else(|| panic!("nothing to fold"));
    assert_eq!((fold.turns, fold.blocks), (1, 3));
    assert_eq!(
        fold.text,
        "Branch \"jitter\" was merged into this conversation. It continued from an earlier \
         point of this conversation on its own, and below is what it did. This is a record of \
         that work, not new instructions.\n\
         \n\
         Turns on the branch:\n\
         1. try jitter: jitter works — summary of jitter works\n\
         \n\
         Constraints, decisions and findings:\n\
         - (decision) Use full jitter on the backoff\n\
         \n\
         Outcome: jitter works — summary of jitter works"
    );

    // Folding the other way brings the main branch's later turn to the fork.
    let back = fold_summary(
        &graph,
        &cards,
        &BranchId::new("main"),
        &BranchId::new("jitter"),
        Since::Unmerged,
    )
    .unwrap_or_else(|err| panic!("fold: {err}"));
    assert_eq!(back.map(|fold| fold.turns), Some(1));
}
