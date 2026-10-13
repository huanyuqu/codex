//! Deterministic grouping of response items into context blocks.
//!
//! Tool calls stay open until their output arrives, and parallel calls may complete out of order.
//! Blocks are released strictly in the order they were opened, so each released block's parent is
//! the block released just before it.

use std::collections::VecDeque;
use std::fmt::Write as _;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use sha2::Digest;
use sha2::Sha256;

use crate::Block;
use crate::BlockId;
use crate::BlockKind;

/// How a message participates in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// A message the user or the assistant actually wrote.
    Conversation,
    /// Harness-injected context that is regenerated rather than recalled.
    Context,
    /// A summary written by compaction.
    CompactionSummary,
}

/// Classifies messages whose meaning depends on harness conventions.
///
/// `kinds` are the per-content-part kinds the harness records with each message; older rollouts
/// do not have them.
pub trait MessageClassifier {
    fn classify(
        &self,
        role: &str,
        content: &[ContentItem],
        kinds: Option<&[ContentItemKind]>,
    ) -> MessageClass;
}

const COMPACTION_SUMMARY_KIND: &str = "compaction.summary";
const USER_KIND_PREFIX: &str = "user.";
const LEGACY_SUMMARY_PREFIX: &str = "Another language model started to solve this problem";
const LEGACY_CONTEXT_PREFIXES: &[&str] = &[
    "<environment_context>",
    "<user_instructions>",
    "# AGENTS.md instructions",
    "<turn_aborted>",
    "<user_shell_command>",
    "<subagent_notification>",
    "<skill>",
    "<recommended_plugins>",
    "<agent_message_board_notification>",
    "<current_time_reminder>",
    "<image_resize_notice>",
];

/// Classifies messages from their recorded content kinds.
///
/// User messages whose parts include a `user.*` kind are conversation, and other kinds mark
/// harness context. Messages from older rollouts without kinds fall back to the text markers
/// those versions wrote.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultClassifier;

impl MessageClassifier for DefaultClassifier {
    fn classify(
        &self,
        role: &str,
        content: &[ContentItem],
        kinds: Option<&[ContentItemKind]>,
    ) -> MessageClass {
        match role {
            "assistant" => return MessageClass::Conversation,
            "user" => {}
            _ => return MessageClass::Context,
        }
        if let Some(kinds) = kinds.filter(|kinds| !kinds.is_empty()) {
            if kinds.iter().any(|kind| kind.0 == COMPACTION_SUMMARY_KIND) {
                return MessageClass::CompactionSummary;
            }
            if kinds
                .iter()
                .any(|kind| kind.0.starts_with(USER_KIND_PREFIX))
            {
                return MessageClass::Conversation;
            }
            return MessageClass::Context;
        }

        let texts: Vec<&str> = content
            .iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text } => Some(text.trim_start()),
                _ => None,
            })
            .collect();
        if texts
            .iter()
            .any(|text| text.starts_with(LEGACY_SUMMARY_PREFIX))
        {
            return MessageClass::CompactionSummary;
        }
        let all_context = !content.is_empty()
            && texts.len() == content.len()
            && texts.iter().all(|text| {
                LEGACY_CONTEXT_PREFIXES
                    .iter()
                    .any(|prefix| text.starts_with(prefix))
            });
        if all_context {
            MessageClass::Context
        } else {
            MessageClass::Conversation
        }
    }
}

/// How one response item affects block boundaries.
enum ItemRole {
    /// A complete block on its own.
    Standalone(BlockKind),
    /// Opens a tool exchange that waits for the output with this call ID.
    Call(String),
    /// Completes the tool exchange with this call ID, when it has one.
    Output(Option<String>),
    /// Attaches to whichever block the same response produces next.
    Reasoning,
    /// A request control that is not part of the conversation.
    Ignored,
}

struct KeyedItem {
    key: String,
    ordinal: u64,
    turn_id: Option<String>,
    bytes: u64,
}

#[derive(Debug)]
struct PendingBlock {
    kind: BlockKind,
    item_keys: Vec<String>,
    turn_id: Option<String>,
    first_ordinal: u64,
    last_ordinal: u64,
    approx_bytes: u64,
    awaiting_call: Option<String>,
    complete: bool,
}

impl PendingBlock {
    fn start(kind: BlockKind, item: KeyedItem) -> Self {
        Self {
            kind,
            item_keys: vec![item.key],
            turn_id: item.turn_id,
            first_ordinal: item.ordinal,
            last_ordinal: item.ordinal,
            approx_bytes: item.bytes,
            awaiting_call: None,
            complete: true,
        }
    }

    fn extend(&mut self, item: KeyedItem) {
        self.item_keys.push(item.key);
        if self.turn_id.is_none() {
            self.turn_id = item.turn_id;
        }
        self.last_ordinal = item.ordinal;
        self.approx_bytes = self.approx_bytes.saturating_add(item.bytes);
    }

    fn seal(self) -> Block {
        let first_key = self.item_keys.first().map_or("", String::as_str);
        Block {
            id: BlockId::for_item_key(first_key),
            kind: self.kind,
            item_keys: self.item_keys,
            turn_id: self.turn_id,
            first_ordinal: self.first_ordinal,
            last_ordinal: self.last_ordinal,
            approx_bytes: self.approx_bytes,
            complete: self.complete,
        }
    }
}

/// Groups a stream of response items into context blocks.
///
/// Feed items in history order with [`Segmenter::push`] and call [`Segmenter::flush`] at turn
/// boundaries so tool calls that never received output are sealed as incomplete.
#[derive(Debug)]
pub struct Segmenter<C> {
    classifier: C,
    open: VecDeque<PendingBlock>,
    reasoning: Option<PendingBlock>,
    last_item_key: Option<String>,
}

impl<C: MessageClassifier> Segmenter<C> {
    pub fn new(classifier: C) -> Self {
        Self {
            classifier,
            open: VecDeque::new(),
            reasoning: None,
            last_item_key: None,
        }
    }

    /// Feeds one item and returns the blocks it released, in order.
    pub fn push(&mut self, ordinal: u64, item: &ResponseItem) -> Vec<Block> {
        let role = self.role(item);
        if matches!(role, ItemRole::Ignored) {
            return Vec::new();
        }
        let keyed = self.key_item(ordinal, item);
        match role {
            ItemRole::Standalone(kind) => {
                let block = match (kind, self.reasoning.take()) {
                    (
                        BlockKind::AssistantMessage | BlockKind::ToolExchange,
                        Some(mut reasoning),
                    ) => {
                        reasoning.kind = kind;
                        reasoning.extend(keyed);
                        reasoning
                    }
                    (_, reasoning) => {
                        // Reasoning followed by a non-assistant item stands on its own.
                        if let Some(reasoning) = reasoning {
                            self.open.push_back(reasoning);
                        }
                        PendingBlock::start(kind, keyed)
                    }
                };
                self.open.push_back(block);
            }
            ItemRole::Call(call_id) => {
                let mut block = match self.reasoning.take() {
                    Some(mut reasoning) => {
                        reasoning.kind = BlockKind::ToolExchange;
                        reasoning.extend(keyed);
                        reasoning
                    }
                    None => PendingBlock::start(BlockKind::ToolExchange, keyed),
                };
                block.awaiting_call = Some(call_id);
                self.open.push_back(block);
            }
            ItemRole::Output(call_id) => {
                let waiting = call_id.as_deref().and_then(|call_id| {
                    self.open
                        .iter()
                        .rposition(|block| block.awaiting_call.as_deref() == Some(call_id))
                });
                match waiting.and_then(|index| self.open.get_mut(index)) {
                    Some(block) => {
                        block.extend(keyed);
                        block.awaiting_call = None;
                    }
                    None => {
                        let mut block = PendingBlock::start(BlockKind::ToolExchange, keyed);
                        block.complete = false;
                        self.open.push_back(block);
                    }
                }
            }
            ItemRole::Reasoning => match &mut self.reasoning {
                Some(reasoning) => reasoning.extend(keyed),
                None => self.reasoning = Some(PendingBlock::start(BlockKind::Reasoning, keyed)),
            },
            ItemRole::Ignored => {}
        }
        self.release()
    }

    /// Seals everything still open, for example at the end of a turn.
    ///
    /// Tool calls that never received output are sealed as incomplete.
    pub fn flush(&mut self) -> Vec<Block> {
        if let Some(reasoning) = self.reasoning.take() {
            self.open.push_back(reasoning);
        }
        for block in &mut self.open {
            if block.awaiting_call.take().is_some() {
                block.complete = false;
            }
        }
        self.release()
    }

    /// Whether any item is held back waiting for a later item.
    pub fn has_open_blocks(&self) -> bool {
        self.reasoning.is_some() || !self.open.is_empty()
    }

    fn release(&mut self) -> Vec<Block> {
        let mut released = Vec::new();
        while self
            .open
            .front()
            .is_some_and(|block| block.awaiting_call.is_none())
        {
            if let Some(block) = self.open.pop_front() {
                released.push(block.seal());
            }
        }
        released
    }

    fn role(&self, item: &ResponseItem) -> ItemRole {
        match item {
            ResponseItem::Message {
                role,
                content,
                internal_chat_message_metadata_passthrough,
                ..
            } => {
                let kinds = internal_chat_message_metadata_passthrough
                    .as_ref()
                    .and_then(|metadata| metadata.content_item_kinds.as_deref());
                match self.classifier.classify(role, content, kinds) {
                    MessageClass::Conversation if role == "assistant" => {
                        ItemRole::Standalone(BlockKind::AssistantMessage)
                    }
                    MessageClass::Conversation => ItemRole::Standalone(BlockKind::UserMessage),
                    MessageClass::Context => ItemRole::Standalone(BlockKind::ContextFragment),
                    MessageClass::CompactionSummary => ItemRole::Standalone(BlockKind::Compaction),
                }
            }
            ResponseItem::AgentMessage { .. } => ItemRole::Standalone(BlockKind::InterAgentMessage),
            ResponseItem::Reasoning { .. } => ItemRole::Reasoning,
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => ItemRole::Call(call_id.clone()),
            ResponseItem::LocalShellCall { call_id, .. }
            | ResponseItem::ToolSearchCall { call_id, .. } => match call_id {
                Some(call_id) => ItemRole::Call(call_id.clone()),
                None => ItemRole::Standalone(BlockKind::ToolExchange),
            },
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::ToolSearchOutput { call_id, .. } => ItemRole::Output(call_id.clone()),
            ResponseItem::CustomToolCallOutput { call_id, .. } => {
                ItemRole::Output(Some(call_id.clone()))
            }
            ResponseItem::WebSearchCall { .. } | ResponseItem::ImageGenerationCall { .. } => {
                ItemRole::Standalone(BlockKind::ToolExchange)
            }
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
                ItemRole::Standalone(BlockKind::Compaction)
            }
            ResponseItem::AdditionalTools { .. } => {
                ItemRole::Standalone(BlockKind::ContextFragment)
            }
            ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger {}
            | ResponseItem::Other => ItemRole::Ignored,
        }
    }

    fn key_item(&mut self, ordinal: u64, item: &ResponseItem) -> KeyedItem {
        let serialized = serde_json::to_vec(item).unwrap_or_default();
        let key = match item.id().filter(|id| !id.is_empty()) {
            Some(id) => id.to_string(),
            None => content_key(self.last_item_key.as_deref(), &serialized),
        };
        self.last_item_key = Some(key.clone());
        KeyedItem {
            key,
            ordinal,
            turn_id: item.turn_id().map(str::to_owned),
            bytes: u64::try_from(serialized.len()).unwrap_or(u64::MAX),
        }
    }
}

/// Key for an item without a stable ID.
///
/// The hash is chained to the previous item's key, so identical content at different points of a
/// history gets different keys, while copies of the same prefix get the same keys.
fn content_key(previous_key: Option<&str>, serialized: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(previous_key.unwrap_or_default().as_bytes());
    hasher.update([0u8]);
    hasher.update(serialized);
    let digest = hasher.finalize();
    let mut key = String::from("h");
    for byte in digest.iter().take(16) {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

#[cfg(test)]
#[path = "segmenter_tests.rs"]
mod tests;
