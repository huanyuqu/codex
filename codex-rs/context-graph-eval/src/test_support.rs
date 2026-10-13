//! A small session for this crate's tests.

use codex_context_graph::BlockId;
use codex_context_graph::BlockKind;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::CARD_VERSION;
use codex_context_graph::history::History;
use codex_context_graph::history::HistoryBlock;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(crate) fn message(role: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Three turns: `u1` asks for a cache, `a1` picks an LRU of 128 entries; `u2` asks for a larger
/// cache, `a2` raises it to 512; `u3` asks about tests, `a3` reports that they pass.
pub(crate) struct Session {
    pub(crate) cards: Vec<BlockCard>,
    pub(crate) items: Vec<Vec<ResponseItem>>,
}

impl Session {
    pub(crate) fn new() -> Self {
        let blocks = [
            ("u1", BlockKind::UserMessage, "Add a cache to the resolver."),
            (
                "a1",
                BlockKind::AssistantMessage,
                "Added an LRU cache with 128 entries.",
            ),
            ("u2", BlockKind::UserMessage, "Make the cache larger."),
            (
                "a2",
                BlockKind::AssistantMessage,
                "Raised the cache to 512 entries.",
            ),
            ("u3", BlockKind::UserMessage, "Do the tests pass?"),
            ("a3", BlockKind::AssistantMessage, "Yes, all 12 tests pass."),
        ];
        let mut session = Self {
            cards: Vec::new(),
            items: Vec::new(),
        };
        for (id, kind, text) in blocks {
            let role = if kind == BlockKind::UserMessage {
                "user"
            } else {
                "assistant"
            };
            session.cards.push(BlockCard {
                block_id: BlockId::from_stored(id),
                kind,
                turn_id: None,
                card_version: CARD_VERSION,
                indexer: "test".to_string(),
                title: text.to_string(),
                summary: String::new(),
                entities: Default::default(),
                tools: Vec::new(),
                outcome: None,
                exit_code: None,
                claims: Vec::new(),
                depends_on: Vec::new(),
                supersedes: Vec::new(),
                tokens: 0,
            });
            session.items.push(vec![message(role, text)]);
        }
        session.cards[3].supersedes = vec![BlockId::from_stored("a1")];
        session
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
}
