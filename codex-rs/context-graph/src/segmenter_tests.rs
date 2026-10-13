use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::assistant;
use crate::test_support::call;
use crate::test_support::developer;
use crate::test_support::message;
use crate::test_support::output;
use crate::test_support::reasoning;
use crate::test_support::user;
use crate::test_support::user_with_kinds;

fn feed(segmenter: &mut Segmenter<DefaultClassifier>, items: &[ResponseItem]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for (ordinal, item) in items.iter().enumerate() {
        let ordinal = u64::try_from(ordinal).unwrap_or(u64::MAX);
        blocks.extend(segmenter.push(ordinal, item));
    }
    blocks
}

fn summary(blocks: &[Block]) -> Vec<(String, BlockKind, Vec<String>, bool)> {
    blocks
        .iter()
        .map(|block| {
            (
                block.id.to_string(),
                block.kind,
                block.item_keys.clone(),
                block.complete,
            )
        })
        .collect()
}

#[test]
fn messages_become_one_block_each() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(
        &mut segmenter,
        &[
            developer("msg_d1", "<permissions instructions>"),
            user("msg_u1", "fix the bug"),
            reasoning("rs_1"),
            assistant("msg_a1", "done"),
        ],
    );
    assert_eq!(
        summary(&blocks),
        vec![
            (
                "blk_msg_d1".to_string(),
                BlockKind::ContextFragment,
                vec!["msg_d1".to_string()],
                true,
            ),
            (
                "blk_msg_u1".to_string(),
                BlockKind::UserMessage,
                vec!["msg_u1".to_string()],
                true,
            ),
            (
                "blk_rs_1".to_string(),
                BlockKind::AssistantMessage,
                vec!["rs_1".to_string(), "msg_a1".to_string()],
                true,
            ),
        ]
    );
    assert!(!segmenter.has_open_blocks());
}

#[test]
fn parallel_calls_release_in_open_order() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(
        &mut segmenter,
        &[
            reasoning("rs_1"),
            call("fc_1", "call_a"),
            call("fc_2", "call_b"),
            output("fco_2", "call_b"),
        ],
    );
    assert_eq!(blocks, Vec::new());

    let blocks = segmenter.push(4, &output("fco_1", "call_a"));
    assert_eq!(
        summary(&blocks),
        vec![
            (
                "blk_rs_1".to_string(),
                BlockKind::ToolExchange,
                vec!["rs_1".to_string(), "fc_1".to_string(), "fco_1".to_string()],
                true,
            ),
            (
                "blk_fc_2".to_string(),
                BlockKind::ToolExchange,
                vec!["fc_2".to_string(), "fco_2".to_string()],
                true,
            ),
        ]
    );
    assert_eq!(blocks[0].first_ordinal, 0);
    assert_eq!(blocks[0].last_ordinal, 4);
}

#[test]
fn user_message_waits_behind_an_open_call() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(
        &mut segmenter,
        &[call("fc_1", "call_a"), user("msg_u2", "also check tests")],
    );
    assert_eq!(blocks, Vec::new());

    let blocks = segmenter.push(2, &output("fco_1", "call_a"));
    let ids: Vec<String> = blocks.iter().map(|block| block.id.to_string()).collect();
    assert_eq!(ids, vec!["blk_fc_1".to_string(), "blk_msg_u2".to_string()]);
}

#[test]
fn reasoning_before_a_user_message_stands_alone() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(&mut segmenter, &[reasoning("rs_1"), user("msg_u1", "stop")]);
    let kinds: Vec<BlockKind> = blocks.iter().map(|block| block.kind).collect();
    assert_eq!(kinds, vec![BlockKind::Reasoning, BlockKind::UserMessage]);
}

#[test]
fn flush_marks_unanswered_and_orphaned_tool_items_incomplete() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let mut blocks = feed(
        &mut segmenter,
        &[output("fco_9", "call_missing"), call("fc_1", "call_a")],
    );
    blocks.extend(segmenter.flush());
    assert_eq!(
        summary(&blocks),
        vec![
            (
                "blk_fco_9".to_string(),
                BlockKind::ToolExchange,
                vec!["fco_9".to_string()],
                false,
            ),
            (
                "blk_fc_1".to_string(),
                BlockKind::ToolExchange,
                vec!["fc_1".to_string()],
                false,
            ),
        ]
    );
    assert!(!segmenter.has_open_blocks());
}

#[test]
fn request_controls_are_ignored() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    assert_eq!(segmenter.push(0, &ResponseItem::Other), Vec::new());
    assert!(!segmenter.has_open_blocks());
}

#[test]
fn classifier_decides_message_kinds() {
    struct Summaries;
    impl MessageClassifier for Summaries {
        fn classify(
            &self,
            role: &str,
            content: &[ContentItem],
            _kinds: Option<&[ContentItemKind]>,
        ) -> MessageClass {
            let summary = content.iter().any(|item| {
                matches!(item, ContentItem::InputText { text } if text.starts_with("SUMMARY"))
            });
            match (role, summary) {
                (_, true) => MessageClass::CompactionSummary,
                ("user" | "assistant", false) => MessageClass::Conversation,
                _ => MessageClass::Context,
            }
        }
    }

    let mut segmenter = Segmenter::new(Summaries);
    let blocks = segmenter.push(0, &user("msg_s", "SUMMARY of earlier work"));
    assert_eq!(blocks[0].kind, BlockKind::Compaction);
}

#[test]
fn items_without_ids_get_chained_content_keys() {
    let items = vec![
        message(None, "user", "same text"),
        message(None, "user", "same text"),
    ];
    let first = feed(&mut Segmenter::new(DefaultClassifier), &items);
    let second = feed(&mut Segmenter::new(DefaultClassifier), &items);

    assert_eq!(first, second);
    assert_ne!(first[0].id, first[1].id);
    assert!(first[0].id.as_str().starts_with("blk_h"));
}

#[test]
fn default_classifier_reads_recorded_kinds() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(
        &mut segmenter,
        &[
            user_with_kinds(
                "msg_env",
                "<environment_context>",
                &["environments.instructions"],
            ),
            user_with_kinds("msg_u1", "refactor the parser", &["user.text"]),
            user_with_kinds("msg_sum", "the parser was split", &["compaction.summary"]),
        ],
    );
    let kinds: Vec<BlockKind> = blocks.iter().map(|block| block.kind).collect();
    assert_eq!(
        kinds,
        vec![
            BlockKind::ContextFragment,
            BlockKind::UserMessage,
            BlockKind::Compaction,
        ]
    );
    assert_eq!(blocks[1].turn_id.as_deref(), Some("turn_1"));
}

#[test]
fn default_classifier_falls_back_to_legacy_markers() {
    let mut segmenter = Segmenter::new(DefaultClassifier);
    let blocks = feed(
        &mut segmenter,
        &[
            user(
                "msg_env",
                "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>",
            ),
            user(
                "msg_sum",
                "Another language model started to solve this problem and produced a summary.",
            ),
            user("msg_u1", "<b>bold</b> is not a context marker"),
        ],
    );
    let kinds: Vec<BlockKind> = blocks.iter().map(|block| block.kind).collect();
    assert_eq!(
        kinds,
        vec![
            BlockKind::ContextFragment,
            BlockKind::Compaction,
            BlockKind::UserMessage,
        ]
    );
}
