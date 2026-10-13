use codex_context_graph::BlockKind;
use codex_context_graph::card::CARD_VERSION;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use pretty_assertions::assert_eq;

use super::*;

fn message(id: &str, role: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }))
}

fn ids(snapshot: &BranchSnapshot) -> Vec<(String, Option<String>)> {
    snapshot
        .blocks
        .iter()
        .zip(&snapshot.origins)
        .map(|(block, origin)| (block.id.to_string(), origin.clone()))
        .collect()
}

#[test]
fn replayed_blocks_stop_at_the_given_head() {
    let items = vec![
        message("msg_u1", "user", "add retries"),
        message("msg_a1", "assistant", "retries added"),
        message("msg_u2", "user", "add jitter"),
    ];
    let blocks = replay_blocks(
        "thread-1",
        &items,
        Some(&BlockId::from_stored("blk_msg_a1")),
    )
    .unwrap_or_else(|err| panic!("replay: {err}"));
    let ids: Vec<String> = blocks
        .iter()
        .map(|block| block.block.id.to_string())
        .collect();
    assert_eq!(ids, vec!["blk_msg_u1", "blk_msg_a1"]);
}

#[test]
fn references_skip_shared_blocks_and_sit_where_they_were_made() {
    let main = vec![
        message("msg_u1", "user", "add retries"),
        message("msg_a1", "assistant", "retries added"),
        message("msg_u3", "user", "add a timeout"),
    ];
    let fork = vec![
        message("msg_u1", "user", "add retries"),
        message("msg_a1", "assistant", "retries added"),
        message("msg_u2", "user", "try jitter"),
        message("msg_a2", "assistant", "jitter works"),
    ];
    let main_blocks = replay_blocks("main", &main, None).unwrap_or_default();
    let fork_blocks = replay_blocks("fork", &fork, None).unwrap_or_default();
    let mut cards = HashMap::new();
    let card = BlockCard {
        block_id: BlockId::from_stored("blk_msg_a2"),
        kind: BlockKind::AssistantMessage,
        turn_id: None,
        card_version: CARD_VERSION,
        indexer: "model".to_string(),
        title: "jitter works".to_string(),
        summary: String::new(),
        entities: Default::default(),
        tools: Vec::new(),
        outcome: None,
        exit_code: None,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        file_stamps: Vec::new(),
        tokens: 0,
    };
    cards.insert(card.block_id.clone(), card);

    let mut snapshot =
        BranchSnapshot::new("graph", BranchId::new("main"), main_blocks, &HashMap::new());
    snapshot.add_reference(
        "jitter",
        Some(&BlockId::from_stored("blk_msg_a1")),
        fork_blocks,
        &cards,
    );
    let referenced = |id: &str| (id.to_string(), Some("jitter".to_string()));
    assert_eq!(
        ids(&snapshot),
        vec![
            ("blk_msg_u1".to_string(), None),
            ("blk_msg_a1".to_string(), None),
            referenced("blk_msg_u2"),
            referenced("blk_msg_a2"),
            ("blk_msg_u3".to_string(), None),
        ]
    );
    // Stored cards are used; the others are extracted.
    assert_eq!(snapshot.cards[3].indexer, "model");
    assert_eq!(snapshot.extracted_cards, 4);
    let history = snapshot.history(None);
    assert_eq!(history.last_turn(), 2);
    assert_eq!(history.origin(2), Some("jitter"));
}
