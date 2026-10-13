//! Branches of a conversation in the context graph: the agent's branch tools, and resumed
//! threads that keep recording into their branch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::MergeMode;
use codex_features::Feature;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

const PLAN_REQUEST: &str = "Plan the release and get the docs written on the side.";
const TASK: &str = "Write the release notes into RELEASE.md.";
const BRANCH_ANSWER: &str = "Release notes written to RELEASE.md.";

fn enable_context_graph(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::ContextGraph)
        .unwrap_or_else(|err| panic!("enable context_graph: {err}"));
}

/// A matcher on the text of a request body.
fn body(
    contains: &'static [&'static str],
    lacks: &'static [&'static str],
) -> impl Fn(&wiremock::Request) -> bool + Send + Sync + 'static {
    move |request: &wiremock::Request| {
        let text = String::from_utf8_lossy(&request.body);
        contains.iter().all(|needle| text.contains(needle))
            && lacks.iter().all(|needle| !text.contains(needle))
    }
}

/// Polls `check` against the store until it holds or five seconds pass, since the graph is
/// written in the background.
async fn eventually<F, Fut>(check: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branch_tools_fork_a_branch_and_merge_its_work() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        body(&[PLAN_REQUEST], &[TASK]),
        sse(vec![
            ev_function_call(
                "call-create",
                "branch_create",
                &json!({ "name": "notes", "task": TASK }).to_string(),
            ),
            ev_completed("r1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[TASK], &["Started branch notes"]),
        sse(vec![
            ev_assistant_message("m-branch", BRANCH_ANSWER),
            ev_completed("r-branch"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&["Started branch notes"], &["Merged branch notes"]),
        sse(vec![
            ev_function_call(
                "call-merge",
                "branch_merge",
                &json!({ "branch": "notes" }).to_string(),
            ),
            ev_completed("r2"),
        ]),
    )
    .await;
    let last = mount_sse_once_match(
        &server,
        body(&["Merged branch notes"], &[]),
        sse(vec![
            ev_assistant_message("m3", "The notes are in."),
            ev_completed("r3"),
        ]),
    )
    .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let mut builder = test_codex().with_config(move |config| {
        config.model = Some("gpt-5.2".to_string());
        config.model_provider = provider;
        config
            .features
            .enable(Feature::ContextGraph)
            .unwrap_or_else(|err| panic!("enable context_graph: {err}"));
    });
    let test = builder.build(&server).await?;
    test.submit_turn(PLAN_REQUEST).await?;

    let requests = last.requests();
    assert_eq!(requests.len(), 1);
    let input = requests[0].body_json()["input"].to_string();
    assert!(
        input.contains("was merged into this conversation") && input.contains(TASK),
        "missing the fold of the branch: {input}"
    );

    let store = LocalContextGraphStore::open(&test.config.sqlite).await?;
    let thread_id = test.session_configured.thread_id.to_string();
    let tree = store
        .branch_tree(&thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the thread is not recorded"))?;
    let rows: Vec<(String, usize)> = tree
        .rows()
        .into_iter()
        .map(|row| (row.name, row.depth))
        .collect();
    assert_eq!(
        rows,
        vec![("main".to_string(), 0), ("notes".to_string(), 1)]
    );
    assert_eq!(
        tree.merges
            .iter()
            .map(|merge| merge.mode)
            .collect::<Vec<_>>(),
        vec![MergeMode::Fold]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_threads_keep_recording_into_their_branch() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "first reply"),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_assistant_message("m2", "second reply"),
                ev_completed("r2"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(enable_context_graph);
    let initial = builder.build(&server).await?;
    let home = Arc::clone(&initial.home);
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no rollout path"))?;
    let thread_id = initial.session_configured.thread_id.to_string();
    initial.submit_turn("before resume").await?;
    let store = LocalContextGraphStore::open(&initial.config.sqlite).await?;
    let turns = |store: LocalContextGraphStore, thread_id: String| async move {
        store
            .branch_tree(&thread_id)
            .await
            .ok()
            .flatten()
            .map(|tree| tree.rows())
            .unwrap_or_default()
    };
    assert!(
        eventually(|| async {
            turns(store.clone(), thread_id.clone())
                .await
                .first()
                .is_some_and(|row| row.turns == 1)
        })
        .await
    );
    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;

    // The builder's settings apply once, so the resumed session needs them again.
    let resumed = builder
        .with_config(enable_context_graph)
        .resume(&server, home, rollout_path)
        .await?;
    assert_eq!(resumed.session_configured.thread_id.to_string(), thread_id);
    resumed.submit_turn("after resume").await?;
    let recorded = eventually(|| async {
        turns(store.clone(), thread_id.clone())
            .await
            .first()
            .is_some_and(|row| row.turns == 2)
    })
    .await;
    assert!(
        recorded,
        "{:?}",
        turns(store.clone(), thread_id.clone()).await
    );
    // Still one branch: the resumed thread did not start a graph of its own.
    let rows = turns(store.clone(), thread_id.clone()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "main");
    Ok(())
}
