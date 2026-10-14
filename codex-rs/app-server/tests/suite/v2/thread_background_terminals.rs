//! `thread/backgroundTerminals/detach`: the commands a turn waits on move to the background, the
//! turn goes on, and the model is told when they exit.

use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ThreadBackgroundTerminalsDetachParams;
use codex_app_server_protocol::ThreadBackgroundTerminalsDetachResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_NOTICE: &str = "<background_command_exit";
const DETACHED_NOTE: &str = "The user moved this command to the background";

fn exec_call(call_id: &str, cmd: &str, yield_time_ms: u64) -> String {
    responses::sse(vec![
        responses::ev_response_created("resp-call"),
        responses::ev_exec_command_call_with_args(
            call_id,
            &json!({ "cmd": cmd, "yield_time_ms": yield_time_ms }),
        ),
        responses::ev_completed("resp-call"),
    ])
}

async fn start(server: &wiremock::MockServer) -> Result<(TempDir, TestAppServer, String)> {
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_sandbox_mode("danger-full-access")
        .enable_feature(Feature::UnifiedExec)
        .disable_feature(Feature::ShellSnapshot)
        .write(codex_home.path())?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse { thread, .. } = app_server
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    Ok((codex_home, app_server, thread.id))
}

async fn start_turn(app_server: &mut TestAppServer, thread_id: &str, text: &str) -> Result<()> {
    let _: TurnStartResponse = app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![V2UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;
    Ok(())
}

async fn detach(app_server: &mut TestAppServer, thread_id: &str) -> Result<()> {
    let response: ThreadBackgroundTerminalsDetachResponse = app_server
        .request(
            |request_id| ClientRequest::ThreadBackgroundTerminalsDetach {
                request_id,
                params: ThreadBackgroundTerminalsDetachParams {
                    thread_id: thread_id.to_string(),
                },
            },
        )
        .await?;
    assert_eq!(response, ThreadBackgroundTerminalsDetachResponse {});
    Ok(())
}

async fn turn_completed(app_server: &mut TestAppServer) -> Result<()> {
    timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_moves_the_waiting_command_to_the_background() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "runs a host-local shell command");

    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        vec![
            exec_call(
                "slow-1",
                "sleep 2; echo SLOW-DONE",
                /*yield_time_ms*/ 30_000,
            ),
            create_final_assistant_message_sse_response("It runs in the background now.")?,
            create_final_assistant_message_sse_response("It finished.")?,
        ],
    )
    .await;
    let (_codex_home, mut app_server, thread_id) = start(&server).await?;

    start_turn(&mut app_server, &thread_id, "run the slow command").await?;
    timeout(READ_TIMEOUT, async {
        loop {
            let started: ItemStartedNotification =
                app_server.read_notification("item/started").await?;
            if matches!(started.item, ThreadItem::CommandExecution { ref id, .. } if id == "slow-1")
            {
                return anyhow::Ok(());
            }
        }
    })
    .await??;
    // The call would wait 30 seconds for the command; the client moves it to the background.
    tokio::time::sleep(Duration::from_millis(300)).await;
    detach(&mut app_server, &thread_id).await?;

    turn_completed(&mut app_server).await?;
    // The command exits after the turn ended, and its exit starts the next turn.
    turn_completed(&mut app_server).await?;

    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    let output = requests[1]
        .function_call_output_text("slow-1")
        .unwrap_or_default();
    assert!(
        output.contains(DETACHED_NOTE),
        "the call output lacks the detach note: {output}"
    );
    let woken = requests[2].body_json()["input"].to_string();
    assert!(
        woken.contains(EXIT_NOTICE) && woken.contains("SLOW-DONE"),
        "the next turn lacks the exit notice: {woken}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_without_a_waiting_command_changes_nothing() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_remote!(Ok(()), "runs a host-local shell command");

    let server = responses::start_mock_server().await;
    let model = responses::mount_sse_sequence(
        &server,
        vec![
            exec_call("quick-1", "echo QUICK-DONE", /*yield_time_ms*/ 10_000),
            create_final_assistant_message_sse_response("Done.")?,
        ],
    )
    .await;
    let (_codex_home, mut app_server, thread_id) = start(&server).await?;

    // Nothing waits yet, so this must not carry over to the next command.
    detach(&mut app_server, &thread_id).await?;
    start_turn(&mut app_server, &thread_id, "run a quick command").await?;
    turn_completed(&mut app_server).await?;
    // The model saw the command exit, so no notice starts another turn.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .function_call_output_text("quick-1")
        .unwrap_or_default();
    assert!(
        output.contains("QUICK-DONE") && !output.contains(DETACHED_NOTE),
        "unexpected call output: {output}"
    );
    Ok(())
}
