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
    assert!(install_live_recorder(
        &live,
        "graph-1",
        &initial,
        sqlite.clone()
    ));

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
    let mut branch = Branch::new(branch_id.clone(), "graph-1");
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

    assert!(install_live_recorder(&live, "graph-1", &[], sqlite.clone()));
    assert!(!install_live_recorder(&live, "graph-2", &[], sqlite));
}
