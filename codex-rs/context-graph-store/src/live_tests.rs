use std::time::Duration;

use codex_context_graph::replay_rollout;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn message(id: &str, role: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }))
}

fn call(id: &str, call_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server(id.to_string())),
        name: "exec_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }))
}

fn output(id: &str, call_id: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(
        ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::from_server(id.to_string())),
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}

fn create_params(thread_id: ThreadId, cwd: &TempDir) -> CreateThreadParams {
    CreateThreadParams {
        creator_user_id: None,
        creator_account_id: None,
        session_id: thread_id.into(),
        thread_id,
        extra_config: None,
        forked_from_id: None,
        parent_thread_id: None,
        source: SessionSource::Exec,
        thread_source: None,
        originator: "context_graph_test".to_string(),
        base_instructions: BaseInstructions::default(),
        dynamic_tools: Vec::new(),
        selected_capability_roots: Vec::new(),
        multi_agent_version: None,
        history_mode: ThreadHistoryMode::Legacy,
        history_base: None,
        subagent_history_start_ordinal: None,
        initial_window_id: "window-1".to_string(),
        runtime_workspace_roots: None,
        metadata: ThreadPersistenceMetadata {
            cwd: Some(cwd.path().to_path_buf()),
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        },
    }
}

fn sqlite_config(home: &TempDir) -> SqliteConfig {
    let home = AbsolutePathBuf::from_absolute_path(home.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    SqliteConfig::new_for_testing(home)
}

async fn live_thread(cwd: &TempDir) -> LiveThread {
    let thread_id = ThreadId::new();
    let store = InMemoryThreadStore::for_id(format!("context-graph-{thread_id}"));
    LiveThread::create(store, create_params(thread_id, cwd))
        .await
        .unwrap_or_else(|err| panic!("create live thread: {err}"))
}

/// Waits for the background writer to store a graph with `blocks` blocks.
async fn stored_graph(
    store: &LocalContextGraphStore,
    graph_id: &str,
    blocks: usize,
) -> ContextGraph {
    for _ in 0..200 {
        if let Ok(Some(graph)) = store.load_graph(graph_id).await
            && graph.len() == blocks
        {
            return graph;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("graph {graph_id} never reached {blocks} blocks");
}

#[tokio::test]
async fn live_recording_matches_a_replay_of_the_same_items() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let live = live_thread(&dir).await;
    let sqlite = sqlite_config(&dir);

    let initial = vec![message("msg_u1", "user", "add retries")];
    assert!(
        install_live_recorder(
            &live,
            GraphMembership::root("graph-1"),
            &initial,
            sqlite.clone(),
            None
        )
        .is_some()
    );

    let appended = vec![
        call("fc_1", "call_1"),
        output("fco_1", "call_1"),
        message("msg_a1", "assistant", "retries added"),
        message("msg_u2", "user", "now add jitter"),
    ];
    live.append_items(&appended)
        .await
        .unwrap_or_else(|err| panic!("append: {err}"));

    let mut expected = ContextGraph::new();
    let branch_id = BranchId::for_thread("graph-1");
    let mut branch = Branch::new(branch_id.clone(), "main");
    branch.thread_id = Some("graph-1".to_string());
    expected
        .add_branch(branch)
        .unwrap_or_else(|err| panic!("add branch: {err}"));
    let all: Vec<RolloutItem> = initial.into_iter().chain(appended).collect();
    replay_rollout(&mut expected, &branch_id, &all, DefaultClassifier)
        .unwrap_or_else(|err| panic!("replay: {err}"));

    let store = LocalContextGraphStore::open(&sqlite)
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let stored = stored_graph(&store, "graph-1", expected.len()).await;
    assert_eq!(stored.export(), expected.export());
}

#[tokio::test]
async fn a_second_recorder_is_not_installed() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let live = live_thread(&dir).await;
    let sqlite = sqlite_config(&dir);

    assert!(
        install_live_recorder(
            &live,
            GraphMembership::root("graph-1"),
            &[],
            sqlite.clone(),
            None
        )
        .is_some()
    );
    assert!(
        install_live_recorder(&live, GraphMembership::root("graph-2"), &[], sqlite, None).is_none()
    );
}

#[tokio::test]
async fn live_indexing_writes_extracted_cards() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let live = live_thread(&dir).await;
    let sqlite = sqlite_config(&dir);

    let initial = vec![message("msg_u1", "user", "add retries")];
    assert!(
        install_live_recorder(
            &live,
            GraphMembership::root("graph-1"),
            &initial,
            sqlite.clone(),
            Some(LiveIndexing::default()),
        )
        .is_some()
    );
    live.append_items(&[
        call("fc_1", "call_1"),
        output("fco_1", "call_1"),
        message("msg_a1", "assistant", "retries added"),
    ])
    .await
    .unwrap_or_else(|err| panic!("append: {err}"));

    let store = LocalContextGraphStore::open(&sqlite)
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let expected = vec![
        ("blk_msg_u1".to_string(), "add retries".to_string()),
        ("blk_fc_1".to_string(), "exec_command".to_string()),
        ("blk_msg_a1".to_string(), "retries added".to_string()),
    ];
    // Cards are ordered by the blocks the graph writer stores on its own task.
    let mut titles = Vec::new();
    for _ in 0..200 {
        titles = store
            .load_cards("graph-1")
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|card| (card.block_id.to_string(), card.title))
            .collect::<Vec<_>>();
        if titles == expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(titles, expected);
}

#[tokio::test]
async fn the_live_graph_holds_the_branch_history_with_items() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let live = live_thread(&dir).await;
    let graph = install_live_recorder(
        &live,
        GraphMembership::root("graph-1"),
        &[message("msg_u1", "user", "add retries")],
        sqlite_config(&dir),
        None,
    )
    .unwrap_or_else(|| panic!("recorder not installed"));
    live.append_items(&[
        call("fc_1", "call_1"),
        output("fco_1", "call_1"),
        message("msg_u2", "user", "now add jitter"),
    ])
    .await
    .unwrap_or_else(|err| panic!("append: {err}"));

    // Appended items are visible as soon as the append returns.
    let history: Vec<(String, usize)> = graph
        .observed_history()
        .into_iter()
        .map(|block| (block.block.id.to_string(), block.items.len()))
        .collect();
    assert_eq!(
        history,
        vec![
            ("blk_msg_u1".to_string(), 1),
            ("blk_fc_1".to_string(), 2),
            ("blk_msg_u2".to_string(), 1),
        ]
    );
    let snapshot = graph
        .snapshot()
        .await
        .unwrap_or_else(|err| panic!("snapshot: {err}"));
    assert_eq!(snapshot.cards[1].title, "exec_command");
    assert_eq!(snapshot.history(None).last_turn(), 2);
}

#[tokio::test]
async fn a_fork_joins_the_graph_of_its_parent() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let sqlite = sqlite_config(&dir);
    let shared = vec![
        message("msg_u1", "user", "add retries"),
        message("msg_a1", "assistant", "retries added"),
    ];

    let parent = live_thread(&dir).await;
    install_live_recorder(
        &parent,
        GraphMembership::root("parent"),
        &shared,
        sqlite.clone(),
        None,
    )
    .unwrap_or_else(|| panic!("parent recorder not installed"));
    let store = LocalContextGraphStore::open(&sqlite)
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    stored_graph(&store, "parent", 2).await;

    let child = live_thread(&dir).await;
    let fork = GraphMembership::fork(
        "child",
        "parent",
        BranchId::for_thread("parent"),
        "try-jitter",
    );
    install_live_recorder(&child, fork, &shared, sqlite.clone(), None)
        .unwrap_or_else(|| panic!("child recorder not installed"));
    child
        .append_items(&[message("msg_u2", "user", "add jitter instead")])
        .await
        .unwrap_or_else(|err| panic!("append: {err}"));
    parent
        .append_items(&[message("msg_u3", "user", "add a timeout")])
        .await
        .unwrap_or_else(|err| panic!("append: {err}"));

    let graph = stored_graph(&store, "parent", 4).await;
    let branch = graph
        .branch(&BranchId::for_thread("child"))
        .unwrap_or_else(|| panic!("child branch missing"));
    assert_eq!(branch.name, "try-jitter");
    assert_eq!(branch.parent, Some(BranchId::for_thread("parent")));
    assert_eq!(
        branch.fork_point.as_ref().map(BlockId::as_str),
        Some("blk_msg_a1")
    );
    assert_eq!(
        graph
            .primary_path(branch.head.as_ref().unwrap_or_else(|| panic!("no head")))
            .unwrap_or_default()
            .iter()
            .map(BlockId::as_str)
            .collect::<Vec<_>>(),
        vec!["blk_msg_u1", "blk_msg_a1", "blk_msg_u2"]
    );
    assert_eq!(
        store
            .branch_of_thread("child")
            .await
            .ok()
            .flatten()
            .map(|(graph_id, branch)| (graph_id, branch.name)),
        Some(("parent".to_string(), "try-jitter".to_string()))
    );
}

#[tokio::test]
async fn threads_find_their_graph_membership() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let sqlite = sqlite_config(&dir);
    let store = LocalContextGraphStore::open(&sqlite)
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let membership = |thread: &'static str, parent: Option<&'static str>| {
        let store = store.clone();
        async move {
            store
                .membership_for_thread(thread, parent)
                .await
                .unwrap_or_else(|err| panic!("membership: {err}"))
        }
    };

    assert_eq!(
        membership("root-1", None).await,
        GraphMembership::root("root-1")
    );
    // A fork of an unrecorded thread starts its own graph.
    assert_eq!(
        membership("fork-a", Some("root-1")).await,
        GraphMembership::root("fork-a")
    );

    let root = live_thread(&dir).await;
    install_live_recorder(
        &root,
        GraphMembership::root("root-1"),
        &[message("msg_u1", "user", "add retries")],
        sqlite.clone(),
        None,
    )
    .unwrap_or_else(|| panic!("recorder not installed"));
    stored_graph(&store, "root-1", 1).await;

    let fork = membership("thread-0123456789", Some("root-1")).await;
    assert_eq!(
        fork,
        GraphMembership::fork(
            "thread-0123456789",
            "root-1",
            BranchId::for_thread("root-1"),
            "fork-23456789"
        )
    );
    // A recorded thread keeps its branch.
    assert_eq!(
        membership("root-1", None).await,
        GraphMembership::root("root-1")
    );
}
