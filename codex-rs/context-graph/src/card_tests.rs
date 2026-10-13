use pretty_assertions::assert_eq;

use super::*;
use crate::extract::extract_facts;
use crate::test_support::developer;
use crate::test_support::exec;
use crate::test_support::output_text;
use crate::test_support::user;

fn card(kind: BlockKind, items: &[ResponseItem]) -> BlockCard {
    let block = Block {
        id: BlockId::from_stored("blk_1"),
        kind,
        item_keys: vec!["1".to_string()],
        turn_id: Some("turn_1".to_string()),
        first_ordinal: 0,
        last_ordinal: 0,
        approx_bytes: 0,
        complete: true,
    };
    BlockCard::extracted(&block, extract_facts(kind, items, Some("/repo")), items)
}

#[test]
fn tool_cards_lead_with_the_command_and_its_result() {
    let card = card(
        BlockKind::ToolExchange,
        &[
            exec("fc_1", "call_1", r#"{"cmd":"cargo test\nmore"}"#),
            output_text(
                "fco_1",
                "call_1",
                "Process exited with code 101\nerror: test failed, to rerun pass `--lib`",
            ),
        ],
    );
    assert_eq!(card.title, "$ cargo test");
    assert_eq!(
        card.summary,
        "exit 101; error: test failed, to rerun pass `--lib`"
    );
    assert_eq!(card.indexer, EXTRACTED_INDEXER);
    assert_eq!(card.card_version, CARD_VERSION);
    assert!(card.wants_model());
    assert_eq!(
        card.catalog_line(),
        "blk_1 [tool_exchange] $ cargo test (failed)"
    );
}

#[test]
fn message_cards_use_the_first_line_and_the_flattened_text() {
    let card = card(
        BlockKind::UserMessage,
        &[user("msg_1", "Add retries\n\n  with   jitter, please")],
    );
    assert_eq!(card.title, "Add retries");
    assert_eq!(card.summary, "Add retries with jitter, please");
}

#[test]
fn harness_context_is_not_sent_to_a_model() {
    let card = card(
        BlockKind::ContextFragment,
        &[developer("msg_1", "<environment_context>")],
    );
    assert!(!card.wants_model());
}

#[test]
fn links_skip_duplicates_and_the_card_itself() {
    let mut card = card(BlockKind::UserMessage, &[user("msg_1", "hi")]);
    card.link(
        Relation::DependsOn,
        [
            BlockId::from_stored("blk_0"),
            BlockId::from_stored("blk_1"),
            BlockId::from_stored("blk_0"),
        ],
    );
    assert_eq!(card.depends_on, vec![BlockId::from_stored("blk_0")]);
}
