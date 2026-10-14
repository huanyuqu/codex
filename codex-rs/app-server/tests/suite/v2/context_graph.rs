use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ContextGraphBlockReadParams;
use codex_app_server_protocol::ContextGraphBlockReadResponse;
use codex_app_server_protocol::ContextGraphBranchPreviewParams;
use codex_app_server_protocol::ContextGraphBranchPreviewResponse;
use codex_app_server_protocol::ContextGraphBranchReadParams;
use codex_app_server_protocol::ContextGraphBranchReadResponse;
use codex_app_server_protocol::ContextGraphBranchStatus;
use codex_app_server_protocol::ContextGraphBranchUpdateParams;
use codex_app_server_protocol::ContextGraphBranchUpdateResponse;
use codex_app_server_protocol::ContextGraphMerge;
use codex_app_server_protocol::ContextGraphMergeMode;
use codex_app_server_protocol::ContextGraphMergeParams;
use codex_app_server_protocol::ContextGraphMergeResponse;
use codex_app_server_protocol::ContextGraphMode;
use codex_app_server_protocol::ContextGraphModeSetParams;
use codex_app_server_protocol::ContextGraphModeSetResponse;
use codex_app_server_protocol::ContextGraphReadParams;
use codex_app_server_protocol::ContextGraphReadResponse;
use codex_app_server_protocol::ContextGraphRecallFeedback;
use codex_app_server_protocol::ContextGraphRecallParams;
use codex_app_server_protocol::ContextGraphRecallResponse;
use codex_app_server_protocol::ContextGraphSearchParams;
use codex_app_server_protocol::ContextGraphSearchResponse;
use codex_app_server_protocol::ContextGraphUpdatedNotification;
use codex_app_server_protocol::ContextGraphViewsReadParams;
use codex_app_server_protocol::ContextGraphViewsReadResponse;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

async fn request<P: Serialize, R: DeserializeOwned>(
    server: &mut TestAppServer,
    method: &str,
    params: P,
) -> Result<R> {
    let id = server
        .send_raw_request(method, Some(serde_json::to_value(params)?))
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, server.read_response(id)).await?
}

async fn read_graph(
    server: &mut TestAppServer,
    thread_id: &str,
) -> Result<ContextGraphReadResponse> {
    request(
        server,
        "contextGraph/read",
        ContextGraphReadParams {
            thread_id: thread_id.to_string(),
        },
    )
    .await
}

/// Reads the graph until `ready` holds, since the graph is written in the background.
async fn read_graph_until(
    server: &mut TestAppServer,
    thread_id: &str,
    ready: impl Fn(&ContextGraphReadResponse) -> bool,
) -> Result<ContextGraphReadResponse> {
    let deadline = tokio::time::Instant::now() + DEFAULT_READ_TIMEOUT;
    loop {
        let graph = read_graph(server, thread_id).await?;
        if ready(&graph) || tokio::time::Instant::now() >= deadline {
            return Ok(graph);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn run_turn(server: &mut TestAppServer, thread_id: &str, text: &str) -> Result<()> {
    timeout(
        DEFAULT_READ_TIMEOUT,
        server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread_id.to_string(),
            input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn context_graph_requests_track_branches_merges_and_modes() -> Result<()> {
    let mock = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&mock.uri())
        .enable_feature(Feature::ContextGraph)
        .write(codex_home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;

    let start = server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread: main, .. } =
        timeout(DEFAULT_READ_TIMEOUT, server.read_response(start)).await??;
    run_turn(&mut server, &main.id, "add retries to the client").await?;
    read_graph_until(&mut server, &main.id, |graph| {
        graph
            .branches
            .first()
            .is_some_and(|branch| branch.turns == 1)
    })
    .await?;

    let fork = server
        .send_thread_fork_request(ThreadForkParams {
            thread_id: main.id.clone(),
            ..Default::default()
        })
        .await?;
    let ThreadForkResponse { thread: forked, .. } =
        timeout(DEFAULT_READ_TIMEOUT, server.read_response(fork)).await??;
    run_turn(&mut server, &forked.id, "try jitter instead").await?;
    let _: ContextGraphBranchUpdateResponse = request(
        &mut server,
        "contextGraph/branch/update",
        ContextGraphBranchUpdateParams {
            thread_id: forked.id.clone(),
            branch: None,
            name: Some("jitter".to_string()),
            purpose: Some("try jitter instead of fixed delays".to_string()),
            archived: None,
        },
    )
    .await?;

    let graph = read_graph_until(&mut server, &main.id, |graph| {
        graph
            .branches
            .iter()
            .any(|branch| branch.name == "jitter" && branch.turns == 1)
    })
    .await?;
    let branches: Vec<(&str, u32, bool)> = graph
        .branches
        .iter()
        .map(|branch| (branch.name.as_str(), branch.depth, branch.current))
        .collect();
    assert_eq!(branches, vec![("main", 0, true), ("jitter", 1, false)]);
    assert_eq!(
        graph.branches[1].purpose.as_deref(),
        Some("try jitter instead of fixed delays")
    );
    assert_eq!(graph.mode, Some(ContextGraphMode::Full));

    // The branch's turns: the one it inherited and its own.
    let read: ContextGraphBranchReadResponse = request(
        &mut server,
        "contextGraph/branch/read",
        ContextGraphBranchReadParams {
            thread_id: main.id.clone(),
            branch: "jitter".to_string(),
        },
    )
    .await?;
    let turns: Vec<(u32, bool, &str)> = read
        .turns
        .iter()
        .filter(|turn| turn.number > 0)
        .map(|turn| (turn.number, turn.inherited, turn.title.as_str()))
        .collect();
    assert_eq!(
        turns,
        vec![
            (1, true, "add retries to the client"),
            (2, false, "try jitter instead")
        ]
    );
    let request_block = read
        .turns
        .iter()
        .find(|turn| turn.number == 2)
        .and_then(|turn| turn.blocks.first())
        .map(|block| block.block_id.clone())
        .unwrap_or_default();
    let block: ContextGraphBlockReadResponse = request(
        &mut server,
        "contextGraph/block/read",
        ContextGraphBlockReadParams {
            thread_id: main.id.clone(),
            block_id: request_block,
        },
    )
    .await?;
    assert_eq!(block.branch_name, "jitter");
    assert!(block.text.contains("try jitter instead"), "{block:?}");

    let search: ContextGraphSearchResponse = request(
        &mut server,
        "contextGraph/search",
        ContextGraphSearchParams {
            thread_id: main.id.clone(),
            query: "jitter".to_string(),
            limit: None,
        },
    )
    .await?;
    assert_eq!(
        search
            .hits
            .first()
            .map(|hit| (hit.branch_name.as_str(), hit.turn)),
        Some(("jitter", 2))
    );
    assert_eq!(graph.merges, Vec::<ContextGraphMerge>::new());

    let preview: ContextGraphBranchPreviewResponse = request(
        &mut server,
        "contextGraph/branch/preview",
        ContextGraphBranchPreviewParams {
            thread_id: main.id.clone(),
            branch: "jitter".to_string(),
        },
    )
    .await?;
    assert_eq!(preview.branch_name, "jitter");
    assert_eq!(preview.turns, 1);
    assert!(
        preview
            .summary
            .as_deref()
            .is_some_and(|summary| summary.contains("try jitter instead")),
        "{preview:?}"
    );

    let merged: ContextGraphMergeResponse = request(
        &mut server,
        "contextGraph/merge",
        ContextGraphMergeParams {
            thread_id: main.id.clone(),
            branches: vec!["jitter".to_string()],
            mode: ContextGraphMergeMode::Fold,
            goal: Some("pick a retry policy".to_string()),
            code: false,
        },
    )
    .await?;
    let merged: Vec<(&str, u32)> = merged
        .merged
        .iter()
        .map(|merged| (merged.branch_name.as_str(), merged.turns))
        .collect();
    assert_eq!(merged, vec![("jitter", 1)]);
    // The rename notified about the fork; the merge notifies about the thread merged into.
    let main_id = main.id.clone();
    let updated = timeout(
        DEFAULT_READ_TIMEOUT,
        server.read_stream_until_matching_notification("contextGraph/updated", move |message| {
            message.method == "contextGraph/updated"
                && message.params.as_ref().is_some_and(|params| {
                    serde_json::from_value::<ContextGraphUpdatedNotification>(params.clone())
                        .is_ok_and(|updated| updated.thread_id == main_id)
                })
        }),
    )
    .await??;
    let updated: ContextGraphUpdatedNotification =
        serde_json::from_value(updated.params.unwrap_or_default())?;
    assert_eq!(updated.graph_id, graph.graph_id.clone().unwrap_or_default());

    // Referencing needs recall, so it fails in full mode.
    let reference = server
        .send_raw_request(
            "contextGraph/merge",
            Some(serde_json::to_value(ContextGraphMergeParams {
                thread_id: main.id.clone(),
                branches: vec!["jitter".to_string()],
                mode: ContextGraphMergeMode::Reference,
                goal: None,
                code: false,
            })?),
        )
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        server.read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(
            reference,
        )),
    )
    .await??;
    assert!(error.error.message.contains("recall"), "{error:?}");

    let _: ContextGraphModeSetResponse = request(
        &mut server,
        "contextGraph/mode/set",
        ContextGraphModeSetParams {
            thread_id: main.id.clone(),
            mode: ContextGraphMode::Auto,
        },
    )
    .await?;
    let graph = read_graph(&mut server, &main.id).await?;
    assert_eq!(graph.mode, Some(ContextGraphMode::Auto));
    let jitter = graph
        .branches
        .iter()
        .find(|branch| branch.name == "jitter")
        .map(|branch| branch.branch_id.clone());
    assert_eq!(
        graph
            .merges
            .iter()
            .map(|merge| (Some(merge.source_branch_id.clone()), merge.mode))
            .collect::<Vec<_>>(),
        vec![(jitter, ContextGraphMergeMode::Fold)]
    );

    // Now a reference merge works.
    let referenced: ContextGraphMergeResponse = request(
        &mut server,
        "contextGraph/merge",
        ContextGraphMergeParams {
            thread_id: main.id.clone(),
            branches: vec!["jitter".to_string()],
            mode: ContextGraphMergeMode::Reference,
            goal: None,
            code: false,
        },
    )
    .await?;
    assert_eq!(
        referenced
            .merged
            .first()
            .map(|merged| merged.branch_name.as_str()),
        Some("jitter")
    );

    // Asking for a recall counts the request; the rebuild waits for the next turn.
    let recall: ContextGraphRecallResponse = request(
        &mut server,
        "contextGraph/recall",
        ContextGraphRecallParams {
            thread_id: main.id.clone(),
            question: None,
            rebuild: true,
        },
    )
    .await?;
    assert_eq!(recall.view, None);

    let views: ContextGraphViewsReadResponse = request(
        &mut server,
        "contextGraph/views/read",
        ContextGraphViewsReadParams {
            thread_id: main.id.clone(),
            limit: None,
        },
    )
    .await?;
    assert_eq!(views.views.len(), 0);
    assert_eq!(
        views.feedback,
        Some(ContextGraphRecallFeedback {
            expanded: 0,
            missed: 0,
            requested: 1,
        })
    );

    // Archiving keeps the branch, marked as archived.
    let _: ContextGraphBranchUpdateResponse = request(
        &mut server,
        "contextGraph/branch/update",
        ContextGraphBranchUpdateParams {
            thread_id: main.id.clone(),
            branch: Some("jitter".to_string()),
            name: None,
            purpose: None,
            archived: Some(true),
        },
    )
    .await?;
    let graph = read_graph(&mut server, &main.id).await?;
    assert_eq!(
        graph
            .branches
            .iter()
            .map(|branch| (branch.name.as_str(), branch.status))
            .collect::<Vec<_>>(),
        vec![
            ("main", ContextGraphBranchStatus::Active),
            ("jitter", ContextGraphBranchStatus::Archived)
        ]
    );
    Ok(())
}

#[tokio::test]
async fn context_graph_requests_report_unknown_branches() -> Result<()> {
    let mock = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&mock.uri())
        .enable_feature(Feature::ContextGraph)
        .write(codex_home.path())?;
    let mut server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized_with_timeout(DEFAULT_READ_TIMEOUT)
        .await?;
    let start = server
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } =
        timeout(DEFAULT_READ_TIMEOUT, server.read_response(start)).await??;
    run_turn(&mut server, &thread.id, "hello").await?;
    read_graph_until(&mut server, &thread.id, |graph| !graph.branches.is_empty()).await?;

    let preview = server
        .send_raw_request(
            "contextGraph/branch/preview",
            Some(serde_json::to_value(ContextGraphBranchPreviewParams {
                thread_id: thread.id.clone(),
                branch: "nope".to_string(),
            })?),
        )
        .await?;
    let error = timeout(
        DEFAULT_READ_TIMEOUT,
        server.read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(
            preview,
        )),
    )
    .await??;
    assert!(error.error.message.contains("nope"), "{error:?}");

    // A thread that has not recorded yet reads as an empty graph.
    let graph = read_graph(&mut server, "019a0000-0000-7000-8000-000000000000").await?;
    assert_eq!(graph.graph_id, None);
    assert!(graph.branches.is_empty());
    Ok(())
}
