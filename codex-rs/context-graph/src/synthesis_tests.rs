use pretty_assertions::assert_eq;

use super::*;
use crate::AppendOutcome;
use crate::Block;
use crate::BlockKind;
use crate::test_support::assistant;
use crate::test_support::user;

fn observed(id: &str, kind: BlockKind, text: &str) -> ObservedBlock {
    let item = match kind {
        BlockKind::UserMessage => user(id, text),
        _ => assistant(id, text),
    };
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
        items: vec![item],
        cwd: None,
    }
}

#[test]
fn histories_are_labeled_across_sources_and_split_into_shards() {
    let jitter = vec![
        observed("u2", BlockKind::UserMessage, "try jitter"),
        observed("a2", BlockKind::AssistantMessage, "jitter works"),
    ];
    let sqlite = vec![observed("u5", BlockKind::UserMessage, "try sqlite")];
    let sources = [
        SynthesisSource {
            name: "jitter",
            summary: "1. try jitter",
            blocks: &jitter,
        },
        SynthesisSource {
            name: "sqlite",
            summary: "1. try sqlite",
            blocks: &sqlite,
        },
    ];

    let whole = label_histories(&sources, SHARD_CHARS);
    assert_eq!(
        whole.labels,
        vec![
            BlockId::from_stored("u2"),
            BlockId::from_stored("a2"),
            BlockId::from_stored("u5"),
        ]
    );
    assert_eq!(whole.shards.len(), 2);
    assert_eq!(whole.shards[0].len(), 1);
    assert!(whole.shards[0][0].starts_with("### [B1] user_message\n"));
    assert!(whole.shards[0][0].contains("### [B2] assistant_message\n"));
    assert!(whole.shards[1][0].starts_with("### [B3] user_message\n"));
    assert!(whole.fits_directly());

    // A small shard size puts each block of the first branch in a shard of its own.
    let split = label_histories(&sources, 40);
    assert_eq!(split.shards[0].len(), 2);
    assert_eq!(split.labels.len(), 3);
}

#[test]
fn note_prompts_attribute_histories_and_carry_the_goal() {
    let blocks = vec![observed("u2", BlockKind::UserMessage, "try jitter")];
    let sources = [SynthesisSource {
        name: "jitter",
        summary: "1. try jitter",
        blocks: &blocks,
    }];
    let prompt = note_prompt(
        &sources,
        &["### [B1] user_message\n[user]\ntry jitter".to_string()],
        Some("pick a retry policy"),
    );
    assert!(
        prompt
            .input
            .starts_with("The merge is for: pick a retry policy.")
    );
    assert!(
        prompt
            .input
            .contains("## Branch \"jitter\"\n\n1. try jitter")
    );
    assert!(
        prompt
            .input
            .contains("### Its history\n\n### [B1] user_message")
    );

    let shard = shard_prompt("jitter", "### [B1] …", 2, 3, None);
    assert!(
        shard
            .input
            .starts_with("## Branch \"jitter\", part 2 of 3 of its history")
    );
}

#[test]
fn citations_become_block_ids_and_made_up_labels_go() {
    let labels = vec![BlockId::from_stored("blk_a"), BlockId::from_stored("blk_b")];
    assert_eq!(
        resolve_citations(
            "Jitter works [B2]. Retries were capped [B1][B2], see [B9]. [Bx] stays; [B] too.",
            &labels
        ),
        "Jitter works [blk_b]. Retries were capped [blk_a][blk_b], see. [Bx] stays; [B] too."
    );
}
