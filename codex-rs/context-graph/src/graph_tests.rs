use pretty_assertions::assert_eq;

use super::*;

fn block(id: &str, kind: BlockKind) -> Block {
    Block {
        id: BlockId::for_item_key(id),
        kind,
        item_keys: vec![id.to_string()],
        turn_id: None,
        first_ordinal: 0,
        last_ordinal: 0,
        approx_bytes: 0,
        complete: true,
    }
}

fn blk(id: &str) -> BlockId {
    BlockId::for_item_key(id)
}

fn graph_with_branch(name: &str) -> (ContextGraph, BranchId) {
    let mut graph = ContextGraph::new();
    let branch = BranchId::new(name);
    graph
        .add_branch(Branch::new(branch.clone(), name))
        .unwrap_or_else(|err| panic!("add branch: {err}"));
    (graph, branch)
}

fn append(graph: &mut ContextGraph, branch: &BranchId, id: &str, kind: BlockKind) -> AppendOutcome {
    graph
        .append(branch, block(id, kind))
        .unwrap_or_else(|err| panic!("append {id}: {err}"))
}

#[test]
fn appends_form_a_chain() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "u1", BlockKind::UserMessage);
    append(&mut graph, &main, "a1", BlockKind::AssistantMessage);

    assert_eq!(
        graph.primary_path(&blk("a1")),
        Ok(vec![blk("u1"), blk("a1")])
    );
    assert_eq!(
        graph.branch(&main).and_then(|b| b.head.clone()),
        Some(blk("a1"))
    );
    assert_eq!(graph.validate(), Ok(()));
}

#[test]
fn a_fork_inherits_its_prefix_and_records_the_fork_point() {
    let (mut graph, main) = graph_with_branch("main");
    for id in ["u1", "a1", "u2", "a2"] {
        append(&mut graph, &main, id, BlockKind::UserMessage);
    }
    let fork = BranchId::new("fork");
    graph
        .add_branch(Branch::new(fork.clone(), "fork"))
        .unwrap_or_else(|err| panic!("add fork: {err}"));

    assert_eq!(
        append(&mut graph, &fork, "u1", BlockKind::UserMessage),
        AppendOutcome::Inherited
    );
    assert_eq!(
        append(&mut graph, &fork, "a1", BlockKind::UserMessage),
        AppendOutcome::Inherited
    );
    assert_eq!(
        append(&mut graph, &fork, "f1", BlockKind::UserMessage),
        AppendOutcome::Added
    );

    let fork_branch = graph.branch(&fork).cloned();
    assert_eq!(
        fork_branch.as_ref().and_then(|b| b.fork_point.clone()),
        Some(blk("a1"))
    );
    assert_eq!(fork_branch.and_then(|b| b.parent), Some(main));
    assert_eq!(
        graph.merge_base(&blk("a2"), &blk("f1")),
        Ok(Some(blk("a1")))
    );
    assert_eq!(graph.len(), 5);
    assert_eq!(graph.validate(), Ok(()));
}

#[test]
fn a_known_block_that_does_not_follow_the_head_is_reincluded() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "u1", BlockKind::UserMessage);
    append(&mut graph, &main, "a1", BlockKind::AssistantMessage);

    assert_eq!(
        append(&mut graph, &main, "u1", BlockKind::UserMessage),
        AppendOutcome::Reincluded
    );
    assert_eq!(
        graph.branch(&main).and_then(|b| b.head.clone()),
        Some(blk("a1"))
    );
}

#[test]
fn rollback_keeps_removed_blocks_on_an_abandoned_branch() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "ctx", BlockKind::ContextFragment);
    for (user, reply) in [("u1", "a1"), ("u2", "a2"), ("u3", "a3")] {
        append(&mut graph, &main, user, BlockKind::UserMessage);
        append(&mut graph, &main, reply, BlockKind::AssistantMessage);
    }

    let abandoned = graph
        .rollback(&main, 2)
        .unwrap_or_else(|err| panic!("rollback: {err}"))
        .unwrap_or_else(|| panic!("expected an abandoned branch"));
    assert_eq!(
        graph.branch(&main).and_then(|b| b.head.clone()),
        Some(blk("a1"))
    );
    let abandoned = graph.branch(&abandoned).cloned();
    assert_eq!(
        abandoned.as_ref().map(|b| b.status),
        Some(BranchStatus::RolledBack)
    );
    assert_eq!(
        abandoned.as_ref().and_then(|b| b.head.clone()),
        Some(blk("a3"))
    );
    assert_eq!(abandoned.and_then(|b| b.fork_point), Some(blk("a1")));

    append(&mut graph, &main, "u2b", BlockKind::UserMessage);
    assert_eq!(
        graph.node(&blk("u2b")).map(|node| node.parents.clone()),
        Some(vec![blk("a1")])
    );
    assert_eq!(graph.validate(), Ok(()));
}

#[test]
fn rollback_past_every_user_turn_empties_the_branch() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "u1", BlockKind::UserMessage);
    append(&mut graph, &main, "a1", BlockKind::AssistantMessage);

    let abandoned = graph
        .rollback(&main, 5)
        .unwrap_or_else(|err| panic!("rollback: {err}"));
    assert!(abandoned.is_some());
    assert_eq!(graph.branch(&main).and_then(|b| b.head.clone()), None);
}

#[test]
fn export_round_trips() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "u1", BlockKind::UserMessage);
    append(&mut graph, &main, "a1", BlockKind::AssistantMessage);

    let export = graph.export();
    assert_eq!(export.format, GRAPH_FORMAT);
    let json = serde_json::to_string(&export).unwrap_or_else(|err| panic!("serialize: {err}"));
    let parsed: GraphExport =
        serde_json::from_str(&json).unwrap_or_else(|err| panic!("deserialize: {err}"));
    let imported = ContextGraph::import(parsed).unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(imported.export(), export);
}

#[test]
fn import_rejects_a_parent_recorded_after_its_child() {
    let (mut graph, main) = graph_with_branch("main");
    append(&mut graph, &main, "u1", BlockKind::UserMessage);
    append(&mut graph, &main, "a1", BlockKind::AssistantMessage);
    let mut export = graph.export();
    export.blocks.reverse();

    assert_eq!(
        ContextGraph::import(export).err(),
        Some(GraphError::InvalidParent {
            block: blk("a1"),
            parent: blk("u1"),
        })
    );
}
