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
use codex_protocol::protocol::TurnCompleteEvent;

use crate::BlockId;
use crate::BlockKind;
use crate::card::BlockCard;
use crate::card::CARD_VERSION;
use crate::card::Claim;
use crate::card::ClaimKind;
use crate::extract::BlockEntities;
use crate::extract::Outcome;
use crate::history::History;
use crate::history::HistoryBlock;

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

/// An `exec_command` call with the given JSON arguments.
pub(crate) fn exec(item_id: &str, call_id: &str, arguments: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        encrypted_content: None,
        status: None,
        id: id(item_id),
        name: "exec_command".to_string(),
        namespace: None,
        arguments: arguments.to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// A function call output with the given text.
pub(crate) fn output_text(item_id: &str, call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: id(item_id),
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// A freeform tool call, such as `apply_patch` or a code mode `exec` script.
pub(crate) fn custom_call(item_id: &str, call_id: &str, name: &str, input: &str) -> ResponseItem {
    ResponseItem::CustomToolCall {
        encrypted_content: None,
        id: id(item_id),
        status: None,
        call_id: call_id.to_string(),
        name: name.to_string(),
        namespace: None,
        input: input.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn custom_output(item_id: &str, call_id: &str, text: &str) -> ResponseItem {
    ResponseItem::CustomToolCallOutput {
        id: id(item_id),
        call_id: call_id.to_string(),
        name: None,
        output: FunctionCallOutputPayload::from_text(text.to_string()),
        internal_chat_message_metadata_passthrough: None,
    }
}

pub(crate) fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
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

/// A card with only the fields recall tests look at.
pub(crate) fn card(id: &str, kind: BlockKind, turn_id: Option<&str>, title: &str) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored(id),
        kind,
        turn_id: turn_id.map(str::to_string),
        card_version: CARD_VERSION,
        indexer: "test".to_string(),
        title: title.to_string(),
        summary: format!("summary of {title}"),
        entities: BlockEntities::default(),
        tools: Vec::new(),
        outcome: None,
        exit_code: None,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        tokens: 0,
    }
}

pub(crate) fn claim(kind: ClaimKind, text: &str) -> Claim {
    Claim {
        kind,
        text: text.to_string(),
    }
}

/// Cards and items of a session, for building a [`History`].
#[derive(Default)]
pub(crate) struct Fixture {
    pub(crate) cards: Vec<BlockCard>,
    pub(crate) items: Vec<Vec<ResponseItem>>,
}

impl Fixture {
    pub(crate) fn push(&mut self, card: BlockCard, items: Vec<ResponseItem>) -> &mut BlockCard {
        self.cards.push(card);
        self.items.push(items);
        let last = self.cards.len() - 1;
        &mut self.cards[last]
    }

    pub(crate) fn history(&self) -> History<'_> {
        History::new(
            self.cards
                .iter()
                .zip(&self.items)
                .map(|(card, items)| HistoryBlock {
                    card,
                    items,
                    complete: true,
                })
                .collect(),
        )
    }

    pub(crate) fn index(&self, id: &str) -> usize {
        self.cards
            .iter()
            .position(|card| card.block_id.as_str() == id)
            .unwrap_or_else(|| panic!("no block {id}"))
    }
}

/// Four turns of a session that adds retries to an HTTP client:
///
/// - `ctx`: harness context before the first turn;
/// - turn 1: `u1` asks for retries and sets a constraint, `r1` reads the client, `a1` decides on
///   exponential backoff;
/// - turn 2: `u2` asks for a fixed delay, `a2` decides so and supersedes `a1`, `e2` edits the
///   client and supersedes `r1`;
/// - turn 3: `u3` asks to run the tests, `t3` runs them and fails, `a3` reports it;
/// - turn 4: `u4` asks why the test failed.
pub(crate) fn retry_session() -> Fixture {
    let mut fixture = Fixture::default();
    fixture.push(
        card(
            "ctx",
            BlockKind::ContextFragment,
            None,
            "environment context",
        ),
        vec![developer(
            "ctx",
            "<environment_context>cwd=/repo</environment_context>",
        )],
    );
    fixture
        .push(
            card(
                "u1",
                BlockKind::UserMessage,
                Some("t1"),
                "add retries to the HTTP client",
            ),
            vec![user(
                "u1",
                "Add retries to the HTTP client. Do not change the public API.",
            )],
        )
        .claims = vec![claim(
        ClaimKind::UserConstraint,
        "Do not change the public API",
    )];
    fixture
        .push(
            card(
                "r1",
                BlockKind::ToolExchange,
                Some("t1"),
                "$ cat src/client.rs",
            ),
            vec![
                exec("r1", "c1", r#"{"cmd":"cat src/client.rs"}"#),
                output_text(
                    "r1o",
                    "c1",
                    "pub fn send(request: Request) -> Response {\n    transport.send(request)\n}",
                ),
            ],
        )
        .entities
        .files_read = vec!["src/client.rs".to_string()];
    fixture
        .push(
            card(
                "a1",
                BlockKind::AssistantMessage,
                Some("t1"),
                "plan: exponential backoff",
            ),
            vec![
                reasoning("a1r"),
                assistant("a1", "I will retry send() with exponential backoff."),
            ],
        )
        .claims = vec![claim(ClaimKind::Decision, "Retry with exponential backoff")];
    fixture.push(
        card(
            "u2",
            BlockKind::UserMessage,
            Some("t2"),
            "use a fixed delay instead",
        ),
        vec![user("u2", "Use a fixed delay instead.")],
    );
    let a2 = fixture.push(
        card(
            "a2",
            BlockKind::AssistantMessage,
            Some("t2"),
            "switch to a fixed delay",
        ),
        vec![assistant("a2", "Switched to a fixed 100 ms delay.")],
    );
    a2.claims = vec![claim(
        ClaimKind::Decision,
        "Retry with a fixed 100 ms delay",
    )];
    a2.supersedes = vec![BlockId::from_stored("a1")];
    let e2 = fixture.push(
        card("e2", BlockKind::ToolExchange, Some("t2"), "edit client.rs"),
        vec![
            custom_call(
                "e2",
                "c2",
                "apply_patch",
                "*** Update File: src/client.rs\n+    retry(|| transport.send(request))",
            ),
            custom_output("e2o", "c2", "Success"),
        ],
    );
    e2.entities.files_written = vec!["src/client.rs".to_string()];
    e2.supersedes = vec![BlockId::from_stored("r1")];
    fixture.push(
        card("u3", BlockKind::UserMessage, Some("t3"), "run the tests"),
        vec![user("u3", "Run the tests.")],
    );
    let t3 = fixture.push(
        card("t3", BlockKind::ToolExchange, Some("t3"), "$ cargo test"),
        vec![
            exec("t3", "c3", r#"{"cmd":"cargo test"}"#),
            output_text(
                "t3o",
                "c3",
                "running 4 tests\ntest send_ok ... ok\ntest retry_fixed ... ok\ntest retry_limit ... FAILED\nexpected 3 attempts, got 4\ntest timeout ... ok",
            ),
        ],
    );
    t3.outcome = Some(Outcome::Failure);
    t3.depends_on = vec![BlockId::from_stored("e2")];
    fixture.push(
        card(
            "a3",
            BlockKind::AssistantMessage,
            Some("t3"),
            "retry_limit fails",
        ),
        vec![assistant(
            "a3",
            "retry_limit fails: 4 attempts instead of 3.",
        )],
    );
    fixture.push(
        card(
            "u4",
            BlockKind::UserMessage,
            Some("t4"),
            "why did the test fail",
        ),
        vec![user("u4", "Why did the test fail?")],
    );
    fixture
}
