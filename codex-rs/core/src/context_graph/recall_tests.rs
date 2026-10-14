use codex_context_graph::BlockId;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::CARD_VERSION;
use codex_context_graph::history::HistoryBlock;
use codex_context_graph::view::Pick;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::plan_view;
use std::collections::HashMap;

use codex_context_graph_store::SelectedBlock;
use pretty_assertions::assert_eq;

use super::*;

const SUMMARY: u8 = 1;
const FULL: u8 = 3;

fn card(id: &str, kind: BlockKind) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored(id),
        kind,
        turn_id: None,
        card_version: CARD_VERSION,
        indexer: "test".to_string(),
        title: id.to_string(),
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
    }
}

fn message(role: &str, text: &str) -> ResponseItem {
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

/// Three turns of requests and answers.
fn session() -> (Vec<BlockCard>, Vec<Vec<ResponseItem>>) {
    let blocks = [
        ("u1", BlockKind::UserMessage, "user", "first request"),
        (
            "a1",
            BlockKind::AssistantMessage,
            "assistant",
            "first answer",
        ),
        ("u2", BlockKind::UserMessage, "user", "second request"),
        (
            "a2",
            BlockKind::AssistantMessage,
            "assistant",
            "second answer",
        ),
        ("u3", BlockKind::UserMessage, "user", "third request"),
    ];
    blocks
        .iter()
        .map(|(id, kind, role, text)| (card(id, *kind), vec![message(role, text)]))
        .unzip()
}

fn history<'a>(cards: &'a [BlockCard], items: &'a [Vec<ResponseItem>]) -> History<'a> {
    History::new(
        cards
            .iter()
            .zip(items)
            .map(|(card, items)| HistoryBlock {
                card,
                items,
                complete: true,
                origin: None,
                changed: false,
            })
            .collect(),
    )
}

fn view_picking(history: &History<'_>, ids: &[usize]) -> ContextView {
    let picks: Vec<Pick> = ids
        .iter()
        .map(|index| Pick {
            index: *index,
            presentation: Presentation::Full,
            note: None,
        })
        .collect();
    plan_view(
        history,
        &picks,
        ViewLimits {
            recent_turns: 1,
            ..ViewLimits::default()
        },
    )
}

#[test]
fn requests_default_to_the_latest_user_message() {
    let (cards, items) = session();
    let history = history(&cards, &items);
    assert_eq!(request_text(&history, None), "third request");
    assert_eq!(request_text(&history, Some("  explicit ")), "explicit");
}

#[test]
fn coverage_counts_the_picked_blocks_the_window_holds() {
    let (cards, items) = session();
    let history = history(&cards, &items);
    // The window was rebuilt showing the first turn, and the second turn arrived since.
    let window = RecalledWindow {
        shown: [
            (BlockId::from_stored("u1"), FULL),
            (BlockId::from_stored("a1"), FULL),
        ]
        .into_iter()
        .collect(),
        known: [BlockId::from_stored("u1"), BlockId::from_stored("a1")]
            .into_iter()
            .collect(),
    };
    assert!(window.holds(&BlockId::from_stored("a2"), /*own*/ true, FULL));
    let view = view_picking(&history, &[1, 3]);
    assert_eq!(coverage(&history, &view, &window), (100, Vec::new()));

    let narrow = RecalledWindow {
        shown: HashMap::new(),
        known: cards.iter().map(|card| card.block_id.clone()).collect(),
    };
    // u1 is the first request, which every view shows; a1 is the only block the window lacks.
    let (held, missing) = coverage(&history, &view_picking(&history, &[1]), &narrow);
    assert_eq!((held, missing), (0, vec![0, 1]));

    let after = window_after(&history, &view);
    assert_eq!(after.shown.get(&BlockId::from_stored("a2")), Some(&FULL));
    assert_eq!(after.known.len(), cards.len());
}

#[test]
fn a_window_holds_a_block_only_with_enough_detail() {
    let mut window = RecalledWindow {
        shown: [(BlockId::from_stored("a1"), SUMMARY)]
            .into_iter()
            .collect(),
        known: [BlockId::from_stored("a1")].into_iter().collect(),
    };
    let a1 = BlockId::from_stored("a1");
    assert!(window.holds(&a1, /*own*/ true, SUMMARY));
    assert!(!window.holds(&a1, /*own*/ true, FULL));
    window.show(a1.clone(), FULL);
    assert!(window.holds(&a1, /*own*/ true, FULL));
    // Showing less of it later keeps what the window holds.
    window.show(a1.clone(), SUMMARY);
    assert!(window.holds(&a1, /*own*/ true, FULL));
}

#[test]
fn lexical_matches_are_added_but_do_not_count_toward_coverage() {
    let (cards, items) = session();
    let history = history(&cards, &items);
    let selection = Selection {
        question: "q".to_string(),
        picks: vec![SelectedBlock {
            block_id: BlockId::from_stored("a1"),
            presentation: Presentation::Full,
            note: None,
        }],
        matches: vec![BlockId::from_stored("a2")],
        stats: Default::default(),
    };
    let recall = selection.plan(
        &history,
        ViewLimits {
            recent_turns: 1,
            ..ViewLimits::default()
        },
    );
    // The window shows the first turn in full, and only a summary of a2.
    let window = RecalledWindow {
        shown: [
            (BlockId::from_stored("u1"), FULL),
            (BlockId::from_stored("a1"), FULL),
        ]
        .into_iter()
        .collect(),
        known: cards.iter().map(|card| card.block_id.clone()).collect(),
    };
    assert_eq!(coverage(&history, &recall.view, &window), (100, vec![3]));
}

#[test]
fn an_earlier_selection_serves_requests_whose_best_matches_it_shows() {
    let (mut cards, items) = session();
    cards[1].title = "retry backoff in the HTTP client".to_string();
    cards[3].title = "timeout of the HTTP client".to_string();
    let history = history(&cards, &items);
    let limits = ViewLimits {
        recent_turns: 1,
        ..ViewLimits::default()
    };
    let selection = |ids: &[&str]| Selection {
        question: "earlier request".to_string(),
        picks: ids
            .iter()
            .map(|id| SelectedBlock {
                block_id: BlockId::from_stored(*id),
                presentation: Presentation::Full,
                note: None,
            })
            .collect(),
        matches: Vec::new(),
        stats: Default::default(),
    };

    assert!(serves(
        &selection(&["a1"]),
        &history,
        "tune backoff",
        limits,
        /*window*/ None,
    ));
    assert!(!serves(
        &selection(&["a1"]),
        &history,
        "raise timeout",
        limits,
        /*window*/ None,
    ));
    // What the window holds counts too.
    let window = RecalledWindow {
        shown: [(BlockId::from_stored("a2"), SUMMARY)]
            .into_iter()
            .collect(),
        known: cards.iter().map(|card| card.block_id.clone()).collect(),
    };
    assert!(serves(
        &selection(&["a1"]),
        &history,
        "raise timeout",
        limits,
        Some(&window),
    ));
    // A request with nothing to match is served by any earlier selection.
    assert!(serves(
        &selection(&[]),
        &history,
        "go on",
        limits,
        /*window*/ None,
    ));
}
