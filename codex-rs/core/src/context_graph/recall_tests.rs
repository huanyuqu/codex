use codex_context_graph::BlockId;
use codex_context_graph::card::BlockCard;
use codex_context_graph::card::CARD_VERSION;
use codex_context_graph::history::HistoryBlock;
use codex_context_graph::view::Pick;
use codex_context_graph::view::Presentation;
use codex_context_graph::view::plan_view;
use pretty_assertions::assert_eq;

use super::*;

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
        shown: [BlockId::from_stored("u1"), BlockId::from_stored("a1")]
            .into_iter()
            .collect(),
        known: [BlockId::from_stored("u1"), BlockId::from_stored("a1")]
            .into_iter()
            .collect(),
    };
    assert!(window.holds(&BlockId::from_stored("a2"), /*own*/ true));
    let view = view_picking(&history, &[1, 3]);
    assert_eq!(coverage(&history, &view, &window), (100, Vec::new()));

    let narrow = RecalledWindow {
        shown: HashSet::new(),
        known: cards.iter().map(|card| card.block_id.clone()).collect(),
    };
    // u1 is the first request, which every view shows; a1 is the only block the window lacks.
    let (held, missing) = coverage(&history, &view_picking(&history, &[1]), &narrow);
    assert_eq!((held, missing), (0, vec![0, 1]));

    let after = window_after(&history, &view);
    assert!(after.shown.contains(&BlockId::from_stored("a2")));
    assert_eq!(after.known.len(), cards.len());
}
