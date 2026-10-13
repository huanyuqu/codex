use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::*;

fn answer(text: &str) -> Value {
    json!({
        "output": [
            { "type": "reasoning", "summary": [] },
            { "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": text }
            ]}
        ],
        "usage": {
            "input_tokens": 1200,
            "input_tokens_details": { "cached_tokens": 1024 },
            "output_tokens": 80
        }
    })
}

#[test]
fn responses_yield_their_text_and_usage() {
    let completion = parse_response(&answer("{\"cards\": []}").to_string())
        .unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(
        completion,
        ModelCompletion {
            text: "{\"cards\": []}".to_string(),
            input_tokens: 1200,
            cached_input_tokens: 1024,
            output_tokens: 80,
        }
    );
    assert!(parse_response(r#"{"output": []}"#).is_err());
}

#[test]
fn messages_before_the_answer_are_ignored() {
    let response = json!({
        "output": [
            { "type": "message", "role": "assistant", "phase": "commentary", "content": [
                { "type": "output_text", "text": "Checks passed; compiling now." }
            ]},
            { "type": "message", "role": "assistant", "content": [
                { "type": "output_text", "text": "{\"cards\": " },
                { "type": "output_text", "text": "[]}" }
            ]}
        ]
    });
    let completion =
        parse_response(&response.to_string()).unwrap_or_else(|err| panic!("parse: {err}"));
    assert_eq!(completion.text, "{\"cards\": []}");
}

#[tokio::test]
async fn structured_requests_retry_server_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_partial_json(json!({
            "model": "small-model",
            "instructions": "index these",
            "reasoning": { "effort": "low" },
            "text": { "format": { "type": "json_schema", "name": "index_cards", "strict": true } },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(answer("{}")))
        .mount(&server)
        .await;

    let model = ResponsesModel::new(
        Endpoint {
            base_url: format!("{}/v1", server.uri()),
            api_key: "test-key".to_string(),
        },
        "small-model",
        Some("low".to_string()),
        Duration::from_secs(10),
    )
    .unwrap_or_else(|err| panic!("client: {err}"));
    let completion = model
        .structured(
            "index these",
            "blocks",
            "index_cards",
            &json!({"type": "object"}),
        )
        .await
        .unwrap_or_else(|err| panic!("request: {err}"));
    assert_eq!(completion.text, "{}");
}
