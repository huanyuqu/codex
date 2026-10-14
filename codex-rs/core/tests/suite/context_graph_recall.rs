//! Sessions that rebuild their history by recall from the context graph.

use anyhow::Result;
use codex_config::types::ContextMode;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
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
