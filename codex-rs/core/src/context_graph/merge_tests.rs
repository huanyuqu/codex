use codex_context_graph::AppendOutcome;
use codex_context_graph::Block;
use codex_context_graph::BlockKind;
use pretty_assertions::assert_eq;

use super::*;

fn observed(id: &str, kind: BlockKind, role: &str, text: &str) -> ObservedBlock {
    ObservedBlock {
        block: Block {
            id: BlockId::from_stored(id),
            kind,
            item_keys: vec![id.to_string()],
            turn_id: None,
            first_ordinal: 0,
            last_ordinal: 0,
            approx_bytes: 0,
            complete: true,
        },
        outcome: AppendOutcome::Added,
        items: vec![ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: role.to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        cwd: None,
    }
}

#[test]
fn histories_render_by_block_and_stop_at_the_limit() {
    let blocks = vec![
        observed("u2", BlockKind::UserMessage, "user", "try jitter"),
        observed(
            "a2",
            BlockKind::AssistantMessage,
            "assistant",
            "jitter works",
        ),
    ];
    assert_eq!(
        render_history(&blocks, usize::MAX),
        "### user_message\n[user]\ntry jitter\n\n### assistant_message\n[assistant]\njitter works"
    );
    assert_eq!(
        render_history(&blocks, 40),
        "### user_message\n[user]\ntry jitter\n\n[... later history omitted ...]"
    );
}

#[test]
fn merge_notes_become_merge_blocks() {
    let id = ResponseItemId::new(codex_context_graph::MERGE_ITEM_PREFIX.trim_end_matches('_'));
    assert!(
        id.as_str()
            .starts_with(codex_context_graph::MERGE_ITEM_PREFIX)
    );
    assert!(
        BlockId::for_item_key(id.as_str())
            .as_str()
            .starts_with("blk_cgmerge_")
    );
}
