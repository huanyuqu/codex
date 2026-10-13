//! Fixture builders shared by this crate's tests.

use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;

fn id(value: &str) -> Option<ResponseItemId> {
    Some(ResponseItemId::from_server(value.to_string()))
}

pub(crate) fn message(item_id: Option<&str>, role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: item_id.and_then(id),
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn user(item_id: &str, text: &str) -> ResponseItem {
    message(Some(item_id), "user", text)
}

pub(crate) fn assistant(item_id: &str, text: &str) -> ResponseItem {
    message(Some(item_id), "assistant", text)
}

pub(crate) fn developer(item_id: &str, text: &str) -> ResponseItem {
    message(Some(item_id), "developer", text)
}

pub(crate) fn reasoning(item_id: &str) -> ResponseItem {
    ResponseItem::Reasoning {
        id: id(item_id),
        summary: Vec::new(),
        content: None,
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn call(item_id: &str, call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: id(item_id),
        name: "exec_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn output(item_id: &str, call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: id(item_id),
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("ok".to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn rollout(item: ResponseItem) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(item))
}

pub(crate) fn rolled_back(num_turns: u32) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns,
    }))
}

pub(crate) fn compacted(replacement_history: Vec<ResponseItem>) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: String::new(),
        replacement_history: Some(
            replacement_history
                .into_iter()
                .map(ResponseItemEnvelope::new)
                .collect(),
        ),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: None,
    })
}

/// A user-role message whose content parts carry the given harness kinds.
pub(crate) fn user_with_kinds(item_id: &str, text: &str, kinds: &[&str]) -> ResponseItem {
    let mut item = user(item_id, text);
    if let ResponseItem::Message {
        internal_chat_message_metadata_passthrough,
        ..
    } = &mut item
    {
        *internal_chat_message_metadata_passthrough =
            Some(InternalChatMessageMetadataPassthrough {
                turn_id: Some("turn_1".to_string()),
                content_item_kinds: Some(
                    kinds
                        .iter()
                        .map(|kind| ContentItemKind((*kind).to_string()))
                        .collect(),
                ),
                ..Default::default()
            });
    }
    item
}
