//! Context graph indexing with the session's own model client.

use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use anyhow::Context as _;
use codex_context_graph::card_prompt::CardPrompt;
use codex_context_graph_store::CardCompletion;
use codex_context_graph_store::CardModel;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_rollout_trace::InferenceTraceContext;
use futures::StreamExt;
use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::compact::content_items_to_text;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::session::session::Session;

/// How long a request waits for the session to exist, such as while a resumed session replays
/// its history before it starts.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes index cards with a small model through the session's model client, so indexing goes
/// to the same provider, with the same authentication and data controls, as the session.
pub(crate) struct SessionCardModel {
    session: watch::Sender<Option<Weak<Session>>>,
    model: String,
}

impl SessionCardModel {
    pub(crate) fn new(model: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            session: watch::Sender::new(None),
            model: model.into(),
        })
    }

    /// Connects the model to its session once the session exists. Requests wait for this, and
    /// fail after the session ends, leaving the extracted cards in place.
    pub(crate) fn attach(&self, session: &Arc<Session>) {
        self.session.send_replace(Some(Arc::downgrade(session)));
    }

    async fn session(&self) -> anyhow::Result<Arc<Session>> {
        let mut attached = self.session.subscribe();
        let session = tokio::time::timeout(ATTACH_TIMEOUT, attached.wait_for(Option::is_some))
            .await
            .context("the session did not start")?
            .context("the session did not start")?
            .clone();
        session
            .as_ref()
            .and_then(Weak::upgrade)
            .context("the session has ended")
    }
}

impl CardModel for SessionCardModel {
    fn name(&self) -> &str {
        &self.model
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a CardPrompt,
    ) -> BoxFuture<'a, anyhow::Result<CardCompletion>> {
        Box::pin(async move {
            let session = self.session().await?;
            session.complete_card_prompt(&self.model, prompt).await
        })
    }
}

impl Session {
    async fn complete_card_prompt(
        &self,
        model: &str,
        card_prompt: &CardPrompt,
    ) -> anyhow::Result<CardCompletion> {
        let config = self.get_config().await;
        let model_info = self
            .services
            .models_manager
            .get_model_info(model, &config.to_models_manager_config())
            .await;
        let prompt = Prompt {
            input: vec![ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: card_prompt.input.clone(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            base_instructions: BaseInstructions {
                text: card_prompt.instructions.clone(),
                provenance: None,
            },
            output_schema: Some(card_prompt.schema.clone()),
            output_schema_strict: true,
            ..Prompt::default()
        };
        let (window_id, _, _) = self.current_window().await;
        let metadata = CodexResponsesMetadata {
            request_kind: Some(CodexResponsesRequestKind::ContextIndex),
            ..CodexResponsesMetadata::new(
                self.installation_id.clone(),
                self.session_id().to_string(),
                self.thread_id.to_string(),
                window_id,
            )
        };

        let mut client_session = self.services.model_client.new_session();
        let mut stream = client_session
            .stream(
                &prompt,
                &model_info,
                &self.services.session_telemetry,
                Some(ReasoningEffort::Low),
                ReasoningSummary::None,
                /*service_tier*/ None,
                &metadata,
                &InferenceTraceContext::disabled(),
            )
            .await?;

        // The answer is the last message; a model may comment in messages before it.
        let mut completion = CardCompletion::default();
        while let Some(event) = stream.next().await.transpose()? {
            match event {
                ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                    if let Some(text) = content_items_to_text(&content) {
                        completion.text = text;
                    }
                }
                ResponseEvent::Completed { token_usage, .. } => {
                    if let Some(usage) = token_usage {
                        completion.input_tokens = u64::try_from(usage.input_tokens).unwrap_or(0);
                        completion.cached_input_tokens =
                            u64::try_from(usage.cached_input_tokens).unwrap_or(0);
                        completion.output_tokens = u64::try_from(usage.output_tokens).unwrap_or(0);
                    }
                    break;
                }
                _ => {}
            }
        }
        Ok(completion)
    }
}
