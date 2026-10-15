//! A minimal non-streaming Responses API client for structured-output requests.

use std::time::Duration;

use anyhow::Context as _;
use codex_context_graph::prompt::ModelPrompt;
use codex_context_graph_store::ModelCompletion;
use codex_context_graph_store::ShadowModel;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::provider::Endpoint;

const ATTEMPTS: u32 = 3;

/// A model behind a Responses endpoint.
pub struct ResponsesModel {
    client: reqwest::Client,
    endpoint: Endpoint,
    model: String,
    reasoning_effort: Option<String>,
}

#[derive(Deserialize)]
struct ResponseBody {
    #[serde(default)]
    output: Vec<Value>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    input_tokens_details: Option<InputTokenDetails>,
}

#[derive(Deserialize)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl ResponsesModel {
    pub fn new(
        endpoint: Endpoint,
        model: impl Into<String>,
        reasoning_effort: Option<String>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Self {
            client,
            endpoint,
            model: model.into(),
            reasoning_effort,
        })
    }

    /// Sends one request whose answer must follow `schema`, retrying throttling and server
    /// errors.
    pub async fn structured(
        &self,
        instructions: &str,
        input: &str,
        schema_name: &str,
        schema: &Value,
    ) -> anyhow::Result<ModelCompletion> {
        let mut body = json!({
            "model": self.model,
            "instructions": instructions,
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": input }],
            }],
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": schema_name,
                    "schema": schema,
                    "strict": true,
                },
            },
            "store": false,
            "stream": false,
        });
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning"] = json!({ "effort": effort });
        }

        let url = format!("{}/responses", self.endpoint.base_url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = self
                .client
                .post(&url)
                .bearer_auth(&self.endpoint.api_key)
                .json(&body)
                .send()
                .await;
            let retryable = match &result {
                Ok(response) => {
                    let status = response.status();
                    status.as_u16() == 429 || status.is_server_error()
                }
                Err(err) => err.is_timeout() || err.is_connect(),
            };
            if retryable && attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(u64::from(attempt) * 2)).await;
                continue;
            }
            let response = result.context("Responses request failed")?;
            let status = response.status();
            let text = response.text().await?;
            if !status.is_success() {
                let snippet: String = text.chars().take(300).collect();
                anyhow::bail!("Responses request failed with {status}: {snippet}");
            }
            return parse_response(&text);
        }
    }
}

/// Collects the output text and usage of a Responses API answer.
pub fn parse_response(text: &str) -> anyhow::Result<ModelCompletion> {
    let body: ResponseBody =
        serde_json::from_str(text).context("Responses answer is not a response object")?;
    // The answer is the last message; a model may comment in messages before it.
    let mut output = String::new();
    for item in &body.output {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let mut text = String::new();
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if part.get("type").and_then(Value::as_str) == Some("output_text")
                && let Some(part_text) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(part_text);
            }
        }
        if !text.is_empty() {
            output = text;
        }
    }
    anyhow::ensure!(!output.is_empty(), "Responses answer has no output text");
    let usage = body.usage;
    Ok(ModelCompletion {
        text: output,
        input_tokens: usage.as_ref().map_or(0, |usage| usage.input_tokens),
        cached_input_tokens: usage
            .as_ref()
            .and_then(|usage| usage.input_tokens_details.as_ref())
            .map_or(0, |details| details.cached_tokens),
        output_tokens: usage.as_ref().map_or(0, |usage| usage.output_tokens),
    })
}

impl ShadowModel for ResponsesModel {
    fn name(&self) -> String {
        self.model.clone()
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(self.structured(
            &prompt.instructions,
            &prompt.input,
            prompt.schema_name,
            &prompt.schema,
        ))
    }
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
