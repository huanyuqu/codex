use pretty_assertions::assert_eq;

use super::*;
use crate::BlockKind;
use crate::card::CARD_VERSION;
use crate::extract::BlockEntities;

fn tool(id: &str, read: &[&str], written: &[&str], command: Option<i32>) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored(id),
        kind: BlockKind::ToolExchange,
        turn_id: None,
        card_version: CARD_VERSION,
        indexer: "extract".to_string(),
        title: id.to_string(),
        summary: String::new(),
        entities: BlockEntities {
            files_read: read.iter().map(|file| (*file).to_string()).collect(),
            files_written: written.iter().map(|file| (*file).to_string()).collect(),
            commands: command
                .map(|_| vec!["cargo test".to_string()])
                .unwrap_or_default(),
            ..BlockEntities::default()
        },
        tools: Vec::new(),
        outcome: None,
        exit_code: command,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        file_stamps: Vec::new(),
        tokens: 1,
    }
}

fn ids(values: &[&str]) -> Vec<BlockId> {
    values
        .iter()
        .map(|value| BlockId::from_stored(*value))
        .collect()
}

#[test]
fn writes_supersede_the_reads_since_the_last_write() {
    let mut tracker = LinkTracker::new();
    let mut cards = vec![
        tool("blk_r1", &["/a.rs"], &[], None),
        tool("blk_r2", &["/a.rs", "/b.rs"], &[], None),
        tool("blk_w1", &[], &["/a.rs"], None),
        tool("blk_r3", &["/a.rs"], &[], None),
        tool("blk_w2", &[], &["/a.rs", "/b.rs"], None),
    ];
    for card in &mut cards {
        tracker.link(card);
    }
    let supersedes: Vec<Vec<BlockId>> = cards.iter().map(|card| card.supersedes.clone()).collect();
    assert_eq!(
        supersedes,
        vec![
            Vec::new(),
            Vec::new(),
            ids(&["blk_r1", "blk_r2"]),
            Vec::new(),
            ids(&["blk_r3", "blk_r2"]),
        ]
    );
}

#[test]
fn commands_depend_on_the_edits_of_the_same_turn() {
    let mut tracker = LinkTracker::new();
    let mut first_edit = tool("blk_w1", &[], &["/a.rs"], None);
    let mut second_edit = tool("blk_w2", &[], &["/b.rs"], None);
    let mut test_run = tool("blk_t1", &[], &[], Some(0));
    let mut rerun = tool("blk_t2", &[], &[], Some(0));
    tracker.link(&mut first_edit);
    tracker.link(&mut second_edit);
    tracker.link(&mut test_run);
    tracker.link(&mut rerun);
    assert_eq!(test_run.depends_on, ids(&["blk_w1", "blk_w2"]));
    assert_eq!(rerun.depends_on, Vec::new());

    let mut edit = tool("blk_w3", &[], &["/c.rs"], None);
    let mut next_turn_run = tool("blk_t3", &[], &[], Some(1));
    tracker.link(&mut edit);
    tracker.end_turn();
    tracker.link(&mut next_turn_run);
    assert_eq!(next_turn_run.depends_on, Vec::new());
}

#[test]
fn recorded_blocks_are_linked_to_but_not_changed() {
    let mut tracker = LinkTracker::new();
    let inherited_read = tool("blk_r1", &["/a.rs"], &[], None);
    let inherited_write = tool("blk_w0", &[], &["/z.rs"], None);
    tracker.record(&inherited_read);
    tracker.record(&inherited_write);
    let mut write = tool("blk_w1", &[], &["/a.rs"], None);
    tracker.link(&mut write);
    assert_eq!(write.supersedes, ids(&["blk_r1"]));
    assert!(inherited_read.supersedes.is_empty());
}
