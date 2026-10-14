//! Commands the model runs in the background: it is told when they exit instead of polling them,
//! and an idle session starts a turn for the news.

use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use serde_json::json;
use tokio::time::Duration;

const NOTICE: &str = "<background_command_exit";

async fn submit(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    Ok(())
}

async fn turn_complete(test: &TestCodex) {
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnComplete(_)),
        Duration::from_secs(20),
    )
    .await;
}

fn exec_call(call_id: &str, args: serde_json::Value) -> String {
    sse(vec![
        ev_response_created("resp-call"),
        ev_function_call(call_id, "exec_command", &args.to_string()),
        ev_completed("resp-call"),
    ])
}

fn answer(id: &str, text: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(id, text),
        ev_completed(id),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_command_wakes_the_idle_session_when_it_exits() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            exec_call(
                "bg-1",
                json!({ "cmd": "sleep 2; echo BG-DONE", "background": true }),
            ),
            answer("resp-started", "Started the build in the background."),
            answer("resp-woken", "The build finished."),
        ],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;

    submit(&test, "run the build").await?;
    turn_complete(&test).await;
    // The command exits after the turn ended; its notice starts the next turn.
    turn_complete(&test).await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    let call_output = requests[1].body_json()["input"].to_string();
    assert!(
        call_output.contains("Running in the background"),
        "missing the background note: {call_output}"
    );
    let woken = requests[2].body_json()["input"].to_string();
    assert!(
        woken.contains(NOTICE) && woken.contains("exited with code 0") && woken.contains("BG-DONE"),
        "missing the exit notice: {woken}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_wake_the_notice_waits_for_the_next_turn() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            exec_call(
                "bg-2",
                json!({ "cmd": "sleep 2; echo QUICK-BG", "background": true }),
            ),
            answer("resp-started", "Started."),
            answer("resp-next", "Noted."),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_session_source(SessionSource::Cli)
        .with_config(|config| {
            config.background_terminal_wake = false;
        });
    let test = builder.build_with_auto_env(&server).await?;

    submit(&test, "start it").await?;
    turn_complete(&test).await;
    // No turn starts on its own.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    assert_eq!(requests.requests().len(), 2);

    submit(&test, "anything new?").await?;
    turn_complete(&test).await;
    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    let next = requests[2].body_json()["input"].to_string();
    assert!(
        next.contains(NOTICE) && next.contains("QUICK-BG"),
        "missing the queued notice: {next}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_user_moves_a_waiting_command_to_the_background() -> Result<()> {
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command");
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            exec_call(
                "fg-1",
                json!({ "cmd": "sleep 2; echo SLOW-DONE", "yield_time_ms": 30000 }),
            ),
            answer("resp-moved", "It runs in the background now."),
            answer("resp-woken", "It finished."),
        ],
    )
    .await;
    let test = test_codex()
        .with_session_source(SessionSource::Cli)
        .build_with_auto_env(&server)
        .await?;

    submit(&test, "run the slow command").await?;
    wait_for_event(
        &test.codex,
        |event| matches!(event, EventMsg::ExecCommandBegin(begin) if begin.call_id == "fg-1"),
    )
    .await;
    // The call would wait 30 seconds; the user moves the command to the background instead.
    tokio::time::sleep(Duration::from_millis(300)).await;
    test.codex.detach_waiting_commands();
    turn_complete(&test).await;
    turn_complete(&test).await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    let call_output = requests[1].body_json()["input"].to_string();
    assert!(
        call_output.contains("The user moved this command to the background"),
        "missing the detach note: {call_output}"
    );
    let woken = requests[2].body_json()["input"].to_string();
    assert!(
        woken.contains(NOTICE) && woken.contains("SLOW-DONE"),
        "missing the exit notice: {woken}"
    );
    Ok(())
}
