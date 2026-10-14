//! Sessions that rebuild their history by recall from the context graph.

use anyhow::Result;
use codex_config::types::ContextMode;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use std::time::Duration;

use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::skip_if_target_windows;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

const FIRST_REQUEST: &str = "Add retries to the HTTP client, but keep the public API unchanged.";
const SECOND_REQUEST: &str = "Now add a timeout as well.";
const RECALL_NOTICE: &str = "Parts of this conversation's history are omitted";

async fn turn(codex: &codex_core::CodexThread, text: &str) -> Result<()> {
    codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_mode_rebuilds_the_history_before_a_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let screening = json!({
        "selected": [{ "card": "C1", "detail": "full", "reason": "states the API constraint" }],
        "uncertain": []
    });
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "Retries added."),
                ev_completed("r1"),
            ]),
            // The screening request of the second turn.
            sse(vec![
                ev_assistant_message("m2", &screening.to_string()),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_assistant_message("m3", "Timeout added."),
                ev_completed("r3"),
            ]),
        ],
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
        config.context_graph.mode = ContextMode::Recall;
        config.context_graph.recent_turns = 1;
        config.context_graph.screen_model = Some("gpt-5.2".to_string());
        config.context_graph.read_model = Some("none".to_string());
        config.context_graph.speculative_recall = false;
    });
    let test = builder.build(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: SECOND_REQUEST.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    // The view recall built is recorded, so the rollout shows what the model saw.
    let recalled = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ContextViewRecalled(_))
    })
    .await;
    let EventMsg::ContextViewRecalled(recalled) = recalled else {
        unreachable!("waited for a recalled view");
    };
    assert!(recalled.rebuilt);
    assert_eq!(recalled.question, SECOND_REQUEST);
    // The first request is shown, as the task the conversation started with.
    assert!(
        recalled
            .entries
            .iter()
            .any(|entry| entry.reason == "task_start" || entry.reason == "selected"),
        "{recalled:?}"
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    // The second request screens the cards of the history for the new request.
    let screen = requests[1].body_json().to_string();
    assert!(screen.contains("You choose the context a coding agent gets"));
    assert!(screen.contains(SECOND_REQUEST));

    // The turn sees the recall notice, the picked first request and the new request.
    let input = requests[2].body_json()["input"].to_string();
    assert!(
        input.contains(RECALL_NOTICE),
        "missing recall notice: {input}"
    );
    assert!(input.contains(FIRST_REQUEST));
    assert!(input.contains(SECOND_REQUEST));
    assert!(input.contains("context_expand"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_expand_reads_a_turn_of_the_history() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("m1", "Retries added."),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_function_call(
                    "call-1",
                    "context_expand",
                    &json!({ "turn": 1 }).to_string(),
                ),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_assistant_message("m3", "The first turn added retries."),
                ev_completed("r3"),
            ]),
        ],
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
        // Auto mode keeps short histories whole but offers the context tools.
        config.context_graph.mode = ContextMode::Auto;
    });
    let test = builder.build(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;
    turn(&test.codex, "What did the first turn do?").await?;

    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    let tools = requests[1].body_json()["tools"].to_string();
    assert!(tools.contains("context_search") && tools.contains("context_expand"));
    let output = requests[2].body_json()["input"].to_string();
    assert!(
        output.contains("(turn 1, user_message)") && output.contains(FIRST_REQUEST),
        "missing expanded turn: {output}"
    );
    Ok(())
}

/// The start of the screening model's instructions, which only screening requests carry.
const SCREENING: &str = "You choose the context a coding agent gets";
const RECALLED_NOTE: &str = "Recalled from earlier in this conversation";
const THIRD_REQUEST: &str = "Why were the retries added?";

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

/// The bodies of the model requests the server received, in order.
async fn request_bodies(server: &wiremock::MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

fn answer(id: &str, text: &str) -> String {
    sse(vec![ev_assistant_message(id, text), ev_completed(id)])
}

fn screening_answer(id: &str, selected: &[&str]) -> String {
    let selected: Vec<_> = selected
        .iter()
        .map(|card| json!({ "card": card, "detail": "full", "reason": "needed" }))
        .collect();
    answer(
        id,
        &json!({ "selected": selected, "uncertain": [] }).to_string(),
    )
}

fn recalling(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::ContextGraph)
        .unwrap_or_else(|err| panic!("enable context_graph: {err}"));
    config.context_graph.mode = ContextMode::Recall;
    config.context_graph.recent_turns = 1;
    config.context_graph.screen_model = Some("gpt-5.2".to_string());
    config.context_graph.read_model = Some("none".to_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_recall_joins_the_turn_between_model_requests() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command");
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        body(
            &[FIRST_REQUEST],
            &[SCREENING, SECOND_REQUEST, THIRD_REQUEST],
        ),
        answer("m1", "Retries added."),
    )
    .await;
    // The second turn's recall answers at once and rebuilds the history.
    mount_sse_once_match(
        &server,
        body(&[SCREENING, SECOND_REQUEST], &[]),
        screening_answer("s2", &[]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[SECOND_REQUEST], &[SCREENING, THIRD_REQUEST]),
        answer("m2", "Timeout added."),
    )
    .await;
    // The third turn's recall takes longer than the turn waits.
    mount_response_once_match(
        &server,
        body(&[SCREENING, THIRD_REQUEST], &[]),
        sse_response(screening_answer(
            "s3",
            &["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"],
        ))
        .set_delay(Duration::from_millis(1_500)),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[THIRD_REQUEST], &[SCREENING, "wait-1"]),
        sse(vec![
            ev_function_call(
                "wait-1",
                "exec_command",
                &json!({ "cmd": "sleep 2", "yield_time_ms": 10_000 }).to_string(),
            ),
            ev_completed("m3"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[THIRD_REQUEST, "wait-1"], &[SCREENING]),
        answer("m4", "They make the client survive flaky networks."),
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        recalling(config);
        config.context_graph.speculative_recall = false;
        config.context_graph.recall_wait_ms = 300;
    });
    let test = builder.build_with_auto_env(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;
    turn(&test.codex, SECOND_REQUEST).await?;
    turn(&test.codex, THIRD_REQUEST).await?;

    let bodies = request_bodies(&server).await;
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains(SCREENING) && body.contains(THIRD_REQUEST))
            .count(),
        1
    );
    let third: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains(THIRD_REQUEST) && !body.contains(SCREENING))
        .collect();
    assert_eq!(third.len(), 2);
    // The first model request went out without the late recall, with summaries of the best
    // lexical matches standing in; the late recall joined before the second.
    let notes = |body: &str| body.matches(RECALLED_NOTE).count();
    assert_eq!(notes(third[0]), 1, "the lexical stand-ins were not added");
    assert_eq!(notes(third[1]), 2, "the late recall was not added");
    assert!(
        third[1].contains("Retries added."),
        "missing the recalled answer"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_next_turn_starts_from_the_recall_made_when_a_turn_ends() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        body(&[FIRST_REQUEST], &[SCREENING, "Go on."]),
        answer("m1", "Retries added."),
    )
    .await;
    // Recall for the next turn reads the ended turn's request and answer.
    mount_sse_once_match(
        &server,
        body(&[SCREENING, FIRST_REQUEST, "Retries added."], &[]),
        screening_answer("s1", &["C1"]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&["Go on."], &[SCREENING]),
        answer("m2", "Going on."),
    )
    .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let mut builder = test_codex().with_config(move |config| {
        config.model = Some("gpt-5.2".to_string());
        config.model_provider = provider;
        recalling(config);
    });
    let test = builder.build(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !request_bodies(&server)
        .await
        .iter()
        .any(|body| body.contains(SCREENING))
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Let the recall read the answer.
    tokio::time::sleep(Duration::from_millis(300)).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Go on.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let recalled = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ContextViewRecalled(_))
    })
    .await;
    let EventMsg::ContextViewRecalled(recalled) = recalled else {
        unreachable!("waited for a recalled view");
    };
    assert!(recalled.rebuilt);
    assert_eq!(recalled.question, "Go on.");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let bodies = request_bodies(&server).await;
    // The request names nothing the earlier recall misses, so it was not screened again: the
    // only screening before its answer is the one made when the first turn ended.
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains(SCREENING) && !body.contains("Going on."))
            .count(),
        1
    );
    let next: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains("Go on.") && !body.contains(SCREENING))
        .collect();
    assert_eq!(next.len(), 1);
    assert!(next[0].contains(RECALL_NOTICE), "missing recall notice");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recall_made_ahead_stands_in_when_the_requests_own_fails() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_target_windows!(Ok(()), "uses a POSIX shell command");
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        body(
            &[FIRST_REQUEST],
            &[SCREENING, SECOND_REQUEST, THIRD_REQUEST],
        ),
        answer("m1", "Retries added."),
    )
    .await;
    // Recall made as the first turn ends picks nothing; it serves the second request, which
    // rebuilds the history from it.
    mount_sse_once_match(
        &server,
        body(
            &[SCREENING, "Retries added."],
            &[SECOND_REQUEST, THIRD_REQUEST, "Timeout added."],
        ),
        screening_answer("s1", &[]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[SECOND_REQUEST], &[SCREENING, THIRD_REQUEST]),
        answer("m2", "Timeout added."),
    )
    .await;
    // Recall made as the second turn ends is still running when the third request comes.
    mount_response_once_match(
        &server,
        body(&[SCREENING, "Timeout added."], &[THIRD_REQUEST]),
        sse_response(screening_answer(
            "s2",
            &["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"],
        ))
        .set_delay(Duration::from_millis(1_500)),
    )
    .await;
    // The third request's own recall fails.
    mount_response_once_match(
        &server,
        body(&[SCREENING, THIRD_REQUEST], &[]),
        wiremock::ResponseTemplate::new(400)
            .set_body_json(json!({ "error": { "message": "bad request" } })),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[THIRD_REQUEST], &[SCREENING, "wait-1"]),
        sse(vec![
            ev_function_call(
                "wait-1",
                "exec_command",
                &json!({ "cmd": "sleep 2", "yield_time_ms": 10_000 }).to_string(),
            ),
            ev_completed("m3"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[THIRD_REQUEST, "wait-1"], &[SCREENING]),
        answer("m4", "They make the client survive flaky networks."),
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.2").with_config(|config| {
        recalling(config);
        config.context_graph.recall_wait_ms = 300;
    });
    let test = builder.build_with_auto_env(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !request_bodies(&server)
        .await
        .iter()
        .any(|body| body.contains(SCREENING))
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    turn(&test.codex, SECOND_REQUEST).await?;
    // Sent at once, while the recall made as the second turn ended still runs.
    turn(&test.codex, THIRD_REQUEST).await?;

    let bodies = request_bodies(&server).await;
    let third: Vec<&String> = bodies
        .iter()
        .filter(|body| body.contains(THIRD_REQUEST) && !body.contains(SCREENING))
        .collect();
    assert_eq!(third.len(), 2);
    // The recall made ahead ended during the turn and stood in for the failed one.
    let notes = |body: &str| body.matches(RECALLED_NOTE).count();
    assert_eq!(
        notes(third[1]),
        notes(third[0]) + 1,
        "the recall made ahead did not stand in"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_recall_of_a_draft_becomes_the_recall_of_the_request() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    mount_sse_once_match(
        &server,
        body(&[FIRST_REQUEST], &[SCREENING, SECOND_REQUEST]),
        answer("m1", "Retries added."),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[SCREENING, SECOND_REQUEST], &[]),
        screening_answer("s-draft", &["C1"]),
    )
    .await;
    mount_sse_once_match(
        &server,
        body(&[SECOND_REQUEST], &[SCREENING]),
        answer("m2", "Timeout added."),
    )
    .await;

    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let mut builder = test_codex().with_config(move |config| {
        config.model = Some("gpt-5.2".to_string());
        config.model_provider = provider;
        recalling(config);
        config.context_graph.speculative_recall = false;
    });
    let test = builder.build(&server).await?;
    turn(&test.codex, FIRST_REQUEST).await?;

    // Too short to be worth recalling for.
    assert!(!test.codex.prefetch_context_recall("Now add").await);
    // The user pauses while writing the request.
    assert!(test.codex.prefetch_context_recall(SECOND_REQUEST).await);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !request_bodies(&server)
        .await
        .iter()
        .any(|body| body.contains(SCREENING))
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: SECOND_REQUEST.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let recalled = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ContextViewRecalled(_))
    })
    .await;
    let EventMsg::ContextViewRecalled(recalled) = recalled else {
        unreachable!("waited for a recalled view");
    };
    assert_eq!(recalled.delivery.as_deref(), Some("draft"));
    assert_eq!(recalled.waited_ms, Some(0));
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    // The request was screened once, as a draft.
    let bodies = request_bodies(&server).await;
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains(SCREENING))
            .count(),
        1
    );
    Ok(())
}
