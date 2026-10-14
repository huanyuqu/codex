use codex_context_graph::AppendOutcome;
use codex_context_graph::BlockKind;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn block(id: &str, kind: BlockKind, ordinal: u64) -> Block {
    Block {
        id: BlockId::from_stored(format!("blk_{id}")),
        kind,
        item_keys: vec![id.to_string()],
        turn_id: Some("turn_1".to_string()),
        first_ordinal: ordinal,
        last_ordinal: ordinal,
        approx_bytes: 120,
        complete: true,
    }
}

async fn open_store(home: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(home.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"))
}

/// Builds a graph with a fork and a rollback, mirroring every change into `store` when given.
async fn build_graph(store: Option<(&LocalContextGraphStore, &str)>) -> ContextGraph {
    let mut graph = ContextGraph::new();
    let main = BranchId::new("br_main");
    let fork = BranchId::new("br_fork");
    graph
        .add_branch(Branch::new(main.clone(), "main"))
        .unwrap_or_else(|err| panic!("add main: {err}"));
    graph
        .add_branch(Branch::new(fork.clone(), "fork"))
        .unwrap_or_else(|err| panic!("add fork: {err}"));

    let steps = [
        (&main, block("u1", BlockKind::UserMessage, 0)),
        (&main, block("a1", BlockKind::AssistantMessage, 1)),
        (&main, block("u2", BlockKind::UserMessage, 2)),
        (&main, block("t2", BlockKind::ToolExchange, 3)),
        (&fork, block("u1", BlockKind::UserMessage, 0)),
        (&fork, block("a1", BlockKind::AssistantMessage, 1)),
        (&fork, block("f1", BlockKind::UserMessage, 2)),
    ];
    for (branch, block) in steps {
        let id = block.id.clone();
        let outcome = graph
            .append(branch, block)
            .unwrap_or_else(|err| panic!("append: {err}"));
        if let Some((store, graph_id)) = store {
            if outcome == AppendOutcome::Added
                && let Some(node) = graph.node(&id)
            {
                store
                    .append_block(graph_id, node)
                    .await
                    .unwrap_or_else(|err| panic!("store block: {err}"));
            }
            if let Some(branch) = graph.branch(branch) {
                store
                    .upsert_branch(graph_id, branch)
                    .await
                    .unwrap_or_else(|err| panic!("store branch: {err}"));
            }
        }
    }

    let abandoned = graph
        .rollback(&main, 1)
        .unwrap_or_else(|err| panic!("rollback: {err}"));
    if let Some((store, graph_id)) = store {
        for branch_id in [Some(main), abandoned].into_iter().flatten() {
            if let Some(branch) = graph.branch(&branch_id) {
                store
                    .upsert_branch(graph_id, branch)
                    .await
                    .unwrap_or_else(|err| panic!("store branch: {err}"));
            }
        }
    }
    graph
}

#[tokio::test]
async fn stored_graphs_round_trip() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    let graph = build_graph(None).await;

    store
        .replace_graph("session-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("replace: {err}"));
    let loaded = store
        .load_graph("session-1")
        .await
        .unwrap_or_else(|err| panic!("load: {err}"))
        .unwrap_or_else(|| panic!("graph should be stored"));

    assert_eq!(loaded.export(), graph.export());
    assert_eq!(
        store.list_graph_ids().await.unwrap_or_default(),
        vec!["session-1".to_string()]
    );
}

#[tokio::test]
async fn incremental_writes_match_a_full_rebuild() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;

    let live = build_graph(Some((&store, "live"))).await;
    store
        .replace_graph("rebuilt", &live)
        .await
        .unwrap_or_else(|err| panic!("replace: {err}"));

    let incremental = store
        .load_graph("live")
        .await
        .ok()
        .flatten()
        .map(|g| g.export());
    let rebuilt = store
        .load_graph("rebuilt")
        .await
        .ok()
        .flatten()
        .map(|g| g.export());
    assert_eq!(incremental, Some(live.export()));
    assert_eq!(incremental, rebuilt);
}

#[tokio::test]
async fn reopening_keeps_data_and_missing_graphs_load_as_none() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let graph = build_graph(None).await;
    open_store(&home)
        .await
        .replace_graph("session-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("replace: {err}"));

    let reopened = open_store(&home).await;
    assert!(
        reopened
            .load_graph("session-1")
            .await
            .ok()
            .flatten()
            .is_some()
    );
    assert!(
        reopened
            .load_graph("missing")
            .await
            .unwrap_or_else(|err| panic!("load: {err}"))
            .is_none()
    );
}

#[tokio::test]
async fn renames_stick_and_merges_are_recorded() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    let graph = build_graph(None).await;
    store
        .replace_graph("graph-1", &graph)
        .await
        .unwrap_or_else(|err| panic!("store graph: {err}"));
    let branch = graph
        .branches()
        .next()
        .cloned()
        .unwrap_or_else(|| panic!("no branch"));

    assert!(
        store
            .rename_branch("graph-1", &branch.id, "retries")
            .await
            .unwrap_or_default()
    );
    // Recording the branch again keeps the new name.
    store
        .upsert_branch("graph-1", &branch)
        .await
        .unwrap_or_else(|err| panic!("upsert: {err}"));
    let stored = store
        .load_graph("graph-1")
        .await
        .ok()
        .flatten()
        .and_then(|graph| graph.branch(&branch.id).cloned());
    assert_eq!(
        stored.map(|branch| branch.name),
        Some("retries".to_string())
    );

    let merge = MergeRecord {
        target: branch.id.clone(),
        source: BranchId::new("br_side"),
        source_head: branch.head.clone(),
        target_head: None,
        merge_block: None,
        mode: MergeMode::Fold,
        goal: None,
        code: None,
    };
    store
        .record_merge("graph-1", &merge)
        .await
        .unwrap_or_else(|err| panic!("record merge: {err}"));
    assert_eq!(store.load_merges("graph-1").await.ok(), Some(vec![merge]));
}

#[tokio::test]
async fn names_given_before_a_thread_records_apply_to_its_branch() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    assert_eq!(
        store
            .name_thread_branch("thread-fork", "try-sqlite")
            .await
            .ok(),
        Some(None)
    );
    assert_eq!(
        store.thread_branch_name("thread-fork").await.ok().flatten(),
        Some("try-sqlite".to_string())
    );

    let mut branch = Branch::new(BranchId::new("br_fork"), "fork-1234");
    branch.thread_id = Some("thread-fork".to_string());
    store
        .upsert_branch("graph-1", &branch)
        .await
        .unwrap_or_else(|err| panic!("upsert: {err}"));
    let name = |store: &LocalContextGraphStore| {
        let store = store.clone();
        async move {
            store
                .branch_of_thread("thread-fork")
                .await
                .ok()
                .flatten()
                .map(|(_, branch)| branch.name)
        }
    };
    assert_eq!(name(&store).await, Some("try-sqlite".to_string()));

    // Naming a recorded thread renames its branch at once.
    assert_eq!(
        store.name_thread_branch("thread-fork", "sqlite").await.ok(),
        Some(Some("graph-1".to_string()))
    );
    assert_eq!(name(&store).await, Some("sqlite".to_string()));
}

#[tokio::test]
async fn branch_settings_are_kept_by_thread_and_survive_recording() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    // Set before the thread records.
    store
        .set_branch_purpose("thread-fork", "  compare SQLite  ")
        .await
        .unwrap_or_else(|err| panic!("purpose: {err}"));
    let mut branch = Branch::new(BranchId::new("br_fork"), "fork");
    branch.thread_id = Some("thread-fork".to_string());
    store
        .upsert_branch("graph-1", &branch)
        .await
        .unwrap_or_else(|err| panic!("upsert: {err}"));
    store
        .set_branch_archived("thread-fork", true)
        .await
        .unwrap_or_else(|err| panic!("archive: {err}"));
    // Recording again keeps the settings.
    store
        .upsert_branch("graph-1", &branch)
        .await
        .unwrap_or_else(|err| panic!("upsert: {err}"));
    let settings = store
        .load_branch_settings("graph-1")
        .await
        .unwrap_or_else(|err| panic!("load: {err}"));
    assert_eq!(
        settings.get("thread-fork"),
        Some(&BranchSettings {
            purpose: Some("compare SQLite".to_string()),
            archived: true,
        })
    );

    // An empty purpose clears it; deleting the thread forgets the settings.
    store
        .set_branch_purpose("thread-fork", " ")
        .await
        .unwrap_or_else(|err| panic!("purpose: {err}"));
    let settings = store.load_branch_settings("graph-1").await.ok();
    assert_eq!(
        settings.and_then(|settings| settings.get("thread-fork").cloned()),
        Some(BranchSettings {
            purpose: None,
            archived: true,
        })
    );
    store
        .delete_thread("thread-fork")
        .await
        .unwrap_or_else(|err| panic!("delete: {err}"));
    store
        .upsert_branch("graph-2", &branch)
        .await
        .unwrap_or_else(|err| panic!("upsert: {err}"));
    assert_eq!(
        store.load_branch_settings("graph-2").await.ok(),
        Some(HashMap::new())
    );
}

#[tokio::test]
async fn recall_feedback_is_counted_by_branch() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    let main = BranchId::new("br_main");
    let block = BlockId::from_stored("blk_a1");
    for feedback in [
        RecallFeedback::Expanded,
        RecallFeedback::Missed,
        RecallFeedback::Expanded,
        RecallFeedback::Requested,
    ] {
        store
            .record_recall_feedback("graph-1", &main, feedback, Some(&block))
            .await
            .unwrap_or_else(|err| panic!("record: {err}"));
    }
    assert_eq!(
        store.recall_feedback_counts("graph-1", &main).await.ok(),
        Some(RecallFeedbackCounts {
            expanded: 2,
            missed: 1,
            requested: 1,
        })
    );
    assert_eq!(
        store
            .recall_feedback_counts("graph-1", &BranchId::new("br_other"))
            .await
            .ok(),
        Some(RecallFeedbackCounts::default())
    );
}

#[tokio::test]
async fn a_merge_of_several_branches_gives_its_block_a_parent_per_branch() {
    let home = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&home).await;
    let merge_block = BlockId::from_stored("blk_cgmerge_1");
    let record = |source: &str, head: &str| MergeRecord {
        target: BranchId::new("br_main"),
        source: BranchId::new(source),
        source_head: Some(BlockId::from_stored(head)),
        target_head: None,
        merge_block: Some(merge_block.clone()),
        mode: MergeMode::Synthesize,
        goal: Some("pick a cache".to_string()),
        code: Some(CodeMerge {
            git_branch: "try-b".to_string(),
            merge_commit: Some("abc".to_string()),
            ..CodeMerge::default()
        }),
    };
    for merge in [record("br_a", "blk_a2"), record("br_b", "blk_b3")] {
        store
            .record_merge("graph-1", &merge)
            .await
            .unwrap_or_else(|err| panic!("record: {err}"));
    }
    let parents: Vec<(i64, String)> = sqlx::query_as(
        "SELECT position, parent_block_id FROM cg_edges WHERE graph_id = 'graph-1' \
         AND child_block_id = ? ORDER BY position",
    )
    .bind(merge_block.as_str())
    .fetch_all(&store.pool)
    .await
    .unwrap_or_else(|err| panic!("edges: {err}"));
    assert_eq!(
        parents,
        vec![(1, "blk_a2".to_string()), (2, "blk_b3".to_string())]
    );
    let merges = store
        .load_merges("graph-1")
        .await
        .unwrap_or_else(|err| panic!("load: {err}"));
    assert_eq!(
        merges,
        vec![record("br_a", "blk_a2"), record("br_b", "blk_b3")]
    );
}
