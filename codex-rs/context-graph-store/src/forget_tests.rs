use codex_context_graph::Block;
use codex_context_graph::BlockKind;
use codex_context_graph::Branch;
use codex_context_graph::ContextGraph;
use codex_context_graph::card::BlockCard;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn block(id: &str) -> Block {
    Block {
        id: BlockId::from_stored(id),
        kind: BlockKind::UserMessage,
        item_keys: vec![id.to_string()],
        turn_id: None,
        first_ordinal: 0,
        last_ordinal: 0,
        approx_bytes: 0,
        complete: true,
    }
}

async fn store(dir: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"))
}

/// `main` (thread `t-main`) holds `u1`, `u2`; `fork` (thread `t-fork`) forks after `u1` and
/// adds `f1`.
async fn forked(store: &LocalContextGraphStore) {
    let main = BranchId::new("main");
    let fork = BranchId::new("fork");
    let mut graph = ContextGraph::new();
    let mut root = Branch::new(main.clone(), "main");
    root.thread_id = Some("t-main".to_string());
    graph
        .add_branch(root)
        .unwrap_or_else(|err| panic!("add main: {err}"));
    for id in ["u1", "u2"] {
        if id == "u2" {
            let mut branch = Branch::new(fork.clone(), "fork");
            branch.thread_id = Some("t-fork".to_string());
            branch.parent = Some(main.clone());
            branch.fork_point = Some(BlockId::from_stored("u1"));
            branch.head = Some(BlockId::from_stored("u1"));
            graph
                .add_branch(branch)
                .unwrap_or_else(|err| panic!("add fork: {err}"));
        }
        graph
            .append(&main, block(id))
            .unwrap_or_else(|err| panic!("append: {err}"));
    }
    graph
        .append(&fork, block("f1"))
        .unwrap_or_else(|err| panic!("append: {err}"));
    store
        .replace_graph("graph-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("store: {err}"));
    for id in ["u1", "u2", "f1"] {
        let node = graph
            .node(&BlockId::from_stored(id))
            .unwrap_or_else(|| panic!("no {id}"));
        let card = BlockCard::extracted(&node.block, Default::default(), &[]);
        store
            .upsert_card("graph-1", &card)
            .await
            .unwrap_or_else(|err| panic!("card: {err}"));
    }
}

async fn block_ids(store: &LocalContextGraphStore) -> Vec<String> {
    let mut ids: Vec<String> = store
        .load_graph("graph-1")
        .await
        .ok()
        .flatten()
        .map(|graph| {
            graph
                .nodes()
                .map(|node| node.block.id.to_string())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

#[tokio::test]
async fn deleting_a_fork_keeps_the_shared_history() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store(&dir).await;
    forked(&store).await;

    store
        .delete_thread("t-fork")
        .await
        .unwrap_or_else(|err| panic!("delete: {err}"));
    assert_eq!(block_ids(&store).await, vec!["u1", "u2"]);
    let cards: Vec<String> = store
        .load_cards("graph-1")
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|card| card.block_id.to_string())
        .collect();
    assert_eq!(cards, vec!["u1", "u2"]);
    assert_eq!(
        store
            .branch_of_thread("t-fork")
            .await
            .ok()
            .flatten()
            .map(|(graph, _)| graph),
        None
    );

    store
        .delete_thread("t-main")
        .await
        .unwrap_or_else(|err| panic!("delete: {err}"));
    assert_eq!(block_ids(&store).await, Vec::<String>::new());
    assert!(
        store
            .load_cards("graph-1")
            .await
            .unwrap_or_default()
            .is_empty()
    );
}
