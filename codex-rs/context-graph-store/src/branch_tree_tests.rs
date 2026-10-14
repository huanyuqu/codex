use codex_context_graph::Block;
use codex_context_graph::BlockKind;
use codex_context_graph::Branch;
use codex_context_graph::card::CARD_VERSION;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

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

fn card(id: &str, kind: BlockKind, title: &str) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored(id),
        kind,
        turn_id: None,
        card_version: CARD_VERSION,
        indexer: "test".to_string(),
        title: title.to_string(),
        summary: String::new(),
        entities: Default::default(),
        tools: Vec::new(),
        outcome: None,
        exit_code: None,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        file_stamps: Vec::new(),
        tokens: 0,
    }
}

/// `main` (thread `t-main`) asks for retries and later a timeout; `jitter` (thread `t-jitter`)
/// forks after the retries and tries jitter.
async fn store_with_fork(dir: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    let store = LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let main = BranchId::new("br-main");
    let jitter = BranchId::new("br-jitter");
    let mut graph = ContextGraph::new();
    let mut root = Branch::new(main.clone(), "main");
    root.thread_id = Some("t-main".to_string());
    graph
        .add_branch(root)
        .unwrap_or_else(|err| panic!("add main: {err}"));
    let blocks = [
        (&main, "u1", BlockKind::UserMessage, "add retries"),
        (&main, "a1", BlockKind::AssistantMessage, "retries added"),
    ];
    for (branch, id, kind, title) in blocks {
        graph
            .append(branch, block(id, kind))
            .unwrap_or_else(|err| panic!("append: {err}"));
        store
            .upsert_card("graph-1", &card(id, kind, title))
            .await
            .unwrap_or_else(|err| panic!("card: {err}"));
    }
    let mut fork = Branch::new(jitter.clone(), "jitter");
    fork.thread_id = Some("t-jitter".to_string());
    fork.parent = Some(main.clone());
    fork.fork_point = Some(BlockId::from_stored("a1"));
    fork.head = Some(BlockId::from_stored("a1"));
    graph
        .add_branch(fork)
        .unwrap_or_else(|err| panic!("add fork: {err}"));
    let blocks = [
        (&jitter, "u2", BlockKind::UserMessage, "try jitter"),
        (&jitter, "a2", BlockKind::AssistantMessage, "jitter works"),
        (&main, "u3", BlockKind::UserMessage, "add a timeout"),
    ];
    for (branch, id, kind, title) in blocks {
        graph
            .append(branch, block(id, kind))
            .unwrap_or_else(|err| panic!("append: {err}"));
        store
            .upsert_card("graph-1", &card(id, kind, title))
            .await
            .unwrap_or_else(|err| panic!("card: {err}"));
    }
    store
        .replace_graph("graph-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("store graph: {err}"));
    store
}

#[tokio::test]
async fn trees_find_branches_and_plan_merges() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store_with_fork(&dir).await;
    let tree = store
        .branch_tree("t-main")
        .await
        .unwrap_or_else(|err| panic!("tree: {err}"))
        .unwrap_or_else(|| panic!("no tree"));

    assert_eq!(tree.current, BranchId::new("br-main"));
    assert_eq!(
        tree.find("@jitter").map(|row| row.branch),
        Some(BranchId::new("br-jitter"))
    );
    assert_eq!(
        tree.find("t-jitter").map(|row| row.name),
        Some("jitter".to_string())
    );
    assert!(matches!(
        tree.plan_merge("main", MergeMode::Fold),
        Err(MergeError::SameBranch)
    ));
    assert!(matches!(
        tree.plan_merge("nope", MergeMode::Fold),
        Err(MergeError::UnknownBranch(_))
    ));

    let plan = tree
        .plan_merge("jitter", MergeMode::Reference)
        .unwrap_or_else(|err| panic!("plan: {err}"));
    assert_eq!(plan.turns, 1);
    assert_eq!(
        plan.record,
        MergeRecord {
            target: BranchId::new("br-main"),
            source: BranchId::new("br-jitter"),
            source_head: Some(BlockId::from_stored("a2")),
            target_head: Some(BlockId::from_stored("u3")),
            merge_block: None,
            mode: MergeMode::Reference,
            goal: None,
            code: None,
        }
    );
    assert!(plan.message.starts_with(
        "Branch \"jitter\" is now referenced by this conversation. It continued from an \
         earlier point of this conversation on its own for 1 turn. Its outcome: jitter works"
    ));

    store
        .record_merge(&tree.graph_id, &plan.record)
        .await
        .unwrap_or_else(|err| panic!("record: {err}"));
    let tree = store
        .branch_tree("t-main")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("no tree"));
    assert_eq!(tree.references().len(), 1);
    assert_eq!(tree.merged_into(&BranchId::new("br-jitter")), vec!["main"]);
}

#[tokio::test]
async fn a_folded_branch_can_still_be_referenced() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store_with_fork(&dir).await;
    let mut graph = store
        .load_graph("graph-1")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("no graph"));
    // The fold's note joins main's history as a merge block.
    graph
        .append(&BranchId::new("br-main"), block("m1", BlockKind::Merge))
        .unwrap_or_else(|err| panic!("append: {err}"));
    store
        .merge_graph("graph-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("store graph: {err}"));
    let tree = store
        .branch_tree("t-main")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("no tree"));
    let mut fold = tree
        .plan_merge("jitter", MergeMode::Fold)
        .unwrap_or_else(|err| panic!("plan: {err}"))
        .record;
    fold.merge_block = Some(BlockId::from_stored("m1"));
    store
        .record_merge(&tree.graph_id, &fold)
        .await
        .unwrap_or_else(|err| panic!("record: {err}"));

    let tree = store
        .branch_tree("t-main")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("no tree"));
    assert!(matches!(
        tree.plan_merge("jitter", MergeMode::Fold),
        Err(MergeError::NothingToMerge(_))
    ));
    let reference = tree
        .plan_merge("jitter", MergeMode::Reference)
        .unwrap_or_else(|err| panic!("plan: {err}"));
    assert_eq!(reference.turns, 1);
}
