use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use pretty_assertions::assert_eq;

use super::*;
use crate::Block;
use crate::BlockId;
use crate::BlockKind;
use crate::Branch;
use crate::BranchStatus;
use crate::DefaultClassifier;
use crate::MessageClass;
use crate::test_support::assistant;
use crate::test_support::call;
use crate::test_support::compacted;
use crate::test_support::developer;
use crate::test_support::output;
use crate::test_support::rolled_back;
use crate::test_support::rollout;
use crate::test_support::user;

struct Summaries;

impl MessageClassifier for Summaries {
    fn classify(
        &self,
        role: &str,
        content: &[ContentItem],
        _kinds: Option<&[ContentItemKind]>,
    ) -> MessageClass {
        let summary = content.iter().any(
            |item| matches!(item, ContentItem::InputText { text } if text.starts_with("SUMMARY")),
        );
        match (role, summary) {
            (_, true) => MessageClass::CompactionSummary,
            ("user" | "assistant", false) => MessageClass::Conversation,
            _ => MessageClass::Context,
        }
    }
}

fn new_graph(branches: &[&str]) -> ContextGraph {
    let mut graph = ContextGraph::new();
    for name in branches {
        graph
            .add_branch(Branch::new(BranchId::new(*name), *name))
            .unwrap_or_else(|err| panic!("add branch: {err}"));
    }
    graph
}

fn head(graph: &ContextGraph, branch: &str) -> Option<BlockId> {
    graph
        .branch(&BranchId::new(branch))
        .and_then(|branch| branch.head.clone())
}

fn kinds(graph: &ContextGraph) -> Vec<(String, BlockKind)> {
    graph
        .nodes()
        .map(|node| {
            let Block { id, kind, .. } = &node.block;
            (id.to_string(), *kind)
        })
        .collect()
}

#[test]
fn replays_a_thread_with_tools_and_a_rollback() {
    let items = vec![
        rollout(developer("msg_d", "<environment_context>")),
        rollout(user("msg_u1", "add retries")),
        rollout(call("fc_1", "call_1")),
        rollout(output("fco_1", "call_1")),
        rollout(assistant("msg_a1", "added")),
        rollout(user("msg_u2", "now use jitter")),
        rollout(assistant("msg_a2", "jittered")),
        rolled_back(1),
        rollout(user("msg_u3", "keep fixed delays")),
    ];
    let mut graph = new_graph(&["main"]);
    let stats = replay_rollout(
        &mut graph,
        &BranchId::new("main"),
        &items,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay: {err}"));

    assert_eq!(
        stats,
        ReplayStats {
            rollout_items: 9,
            blocks_added: 7,
            blocks_inherited: 0,
            blocks_reincluded: 0,
            rollbacks: 1,
        }
    );
    assert_eq!(head(&graph, "main"), Some(BlockId::for_item_key("msg_u3")));
    assert_eq!(
        graph
            .node(&BlockId::for_item_key("msg_u3"))
            .map(|node| node.parents.clone()),
        Some(vec![BlockId::for_item_key("msg_a1")])
    );
    let abandoned: Vec<(String, BranchStatus)> = graph
        .branches()
        .filter(|branch| branch.status == BranchStatus::RolledBack)
        .map(|branch| (branch.id.to_string(), branch.status))
        .collect();
    assert_eq!(
        abandoned,
        vec![("main~rollback-1".to_string(), BranchStatus::RolledBack)]
    );
    assert_eq!(graph.validate(), Ok(()));
}

#[test]
fn compaction_adds_only_the_summary() {
    let items = vec![
        rollout(user("msg_u1", "plan the migration")),
        rollout(assistant("msg_a1", "here is the plan")),
        compacted(vec![
            developer("msg_ctx2", "<environment_context>"),
            user("msg_u1", "plan the migration"),
            user("msg_sum", "SUMMARY the plan has three steps"),
        ]),
        rollout(user("msg_u2", "start step one")),
    ];
    let mut graph = new_graph(&["main"]);
    let stats = replay_rollout(&mut graph, &BranchId::new("main"), &items, Summaries)
        .unwrap_or_else(|err| panic!("replay: {err}"));

    assert_eq!(stats.blocks_reincluded, 1);
    assert_eq!(
        kinds(&graph),
        vec![
            ("blk_msg_u1".to_string(), BlockKind::UserMessage),
            ("blk_msg_a1".to_string(), BlockKind::AssistantMessage),
            ("blk_msg_ctx2".to_string(), BlockKind::ContextFragment),
            ("blk_msg_sum".to_string(), BlockKind::Compaction),
            ("blk_msg_u2".to_string(), BlockKind::UserMessage),
        ]
    );
}

#[test]
fn a_copied_fork_shares_its_prefix_with_the_source() {
    let shared = vec![
        rollout(user("msg_u1", "design the cache")),
        rollout(assistant("msg_a1", "two options")),
    ];
    let mut parent = shared.clone();
    parent.push(rollout(user("msg_u2", "take option A")));
    let mut child = shared;
    child.push(rollout(user("msg_f1", "explore option B")));

    let mut graph = new_graph(&["parent", "child"]);
    replay_rollout(
        &mut graph,
        &BranchId::new("parent"),
        &parent,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay parent: {err}"));
    let stats = replay_rollout(
        &mut graph,
        &BranchId::new("child"),
        &child,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay child: {err}"));

    assert_eq!(stats.blocks_inherited, 2);
    assert_eq!(stats.blocks_added, 1);
    let child = graph.branch(&BranchId::new("child")).cloned();
    assert_eq!(
        child.as_ref().and_then(|branch| branch.fork_point.clone()),
        Some(BlockId::for_item_key("msg_a1"))
    );
    assert_eq!(
        child.and_then(|branch| branch.parent),
        Some(BranchId::new("parent"))
    );
    assert_eq!(graph.len(), 4);
}

#[test]
fn item_by_item_recording_matches_a_full_replay() {
    let items = vec![
        rollout(user("msg_u1", "add retries")),
        rollout(call("fc_1", "call_1")),
        rollout(output("fco_1", "call_1")),
        rollout(assistant("msg_a1", "added")),
        rolled_back(1),
        rollout(user("msg_u2", "different approach")),
    ];
    let mut replayed = new_graph(&["main"]);
    replay_rollout(
        &mut replayed,
        &BranchId::new("main"),
        &items,
        DefaultClassifier,
    )
    .unwrap_or_else(|err| panic!("replay: {err}"));

    let mut live = new_graph(&["main"]);
    let mut recorder = GraphRecorder::new(BranchId::new("main"), DefaultClassifier);
    let mut added = Vec::new();
    for item in &items {
        for change in recorder
            .observe(&mut live, item)
            .unwrap_or_else(|err| panic!("observe: {err}"))
        {
            if let GraphChange::BlockAdded(id) = change {
                added.push(id.to_string());
            }
        }
    }
    recorder
        .finish(&mut live)
        .unwrap_or_else(|err| panic!("finish: {err}"));

    assert_eq!(live.export(), replayed.export());
    assert_eq!(
        added,
        vec![
            "blk_msg_u1".to_string(),
            "blk_fc_1".to_string(),
            "blk_msg_a1".to_string(),
            "blk_msg_u2".to_string(),
        ]
    );
}
