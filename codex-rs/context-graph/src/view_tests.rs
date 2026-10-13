use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::*;
use crate::test_support::Fixture;
use crate::test_support::retry_session;

fn limits(recent_turns: usize) -> ViewLimits {
    ViewLimits {
        recent_turns,
        ..ViewLimits::default()
    }
}

fn pick(session: &Fixture, id: &str, presentation: Presentation) -> Pick {
    Pick {
        index: session.index(id),
        presentation,
        note: None,
    }
}

fn shown(view: &ContextView) -> Vec<(&str, Presentation, Reason)> {
    view.entries
        .iter()
        .map(|entry| {
            (
                entry.block_id.as_str(),
                entry.presentation.clone(),
                entry.reason,
            )
        })
        .collect()
}

fn text(item: &ResponseItem) -> String {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let text: Vec<&str> = content
                .iter()
                .filter_map(|content| match content {
                    ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect();
            format!("{role}: {}", text.join(""))
        }
        ResponseItem::FunctionCall { call_id, .. } => format!("call {call_id}"),
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            format!("output {}", call_id.as_deref().unwrap_or_default())
        }
        ResponseItem::Reasoning { .. } => "reasoning".to_string(),
        other => format!("{other:?}"),
    }
}

#[test]
fn views_keep_recent_turns_the_task_picks_and_their_dependencies() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(
        &history,
        &[pick(&session, "t3", Presentation::Full)],
        limits(1),
    );

    assert_eq!(
        shown(&view),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            ("e2", Presentation::Summary, Reason::Dependency),
            ("t3", Presentation::Full, Reason::Selected),
            ("u4", Presentation::Full, Reason::Recent),
        ]
    );
    let ledger: Vec<(&str, usize, &str)> = view
        .ledger
        .iter()
        .map(|claim| (claim.block_id.as_str(), claim.turn, claim.text.as_str()))
        .collect();
    // The exponential backoff decision was reversed in turn 2, so it is not in force.
    assert_eq!(
        ledger,
        vec![
            ("u1", 1, "Do not change the public API"),
            ("a2", 2, "Retry with a fixed 100 ms delay"),
        ]
    );
    assert_eq!(
        view.map,
        vec![
            "turn 1: add retries to the HTTP client → plan: exponential backoff [1 shown below]",
            "turn 2: use a fixed delay instead → switch to a fixed delay [1 shown below]",
            "turn 3: run the tests → retry_limit fails [1 shown below]",
        ]
    );
    assert_eq!((view.shrunk, view.dropped), (0, 0));
    assert!(view.tokens <= view.budget_tokens);
}

#[test]
fn outdated_picks_are_summarized_next_to_what_replaced_them() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(
        &history,
        &[
            pick(&session, "r1", Presentation::Full),
            pick(&session, "a1", Presentation::Full),
        ],
        limits(1),
    );

    assert_eq!(
        shown(&view),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            ("r1", Presentation::Summary, Reason::Selected),
            ("a1", Presentation::Summary, Reason::Selected),
            ("a2", Presentation::Summary, Reason::Successor),
            ("e2", Presentation::Summary, Reason::Successor),
            ("u4", Presentation::Full, Reason::Recent),
        ]
    );
    let superseded: Vec<Option<&str>> = view.entries[1..3]
        .iter()
        .map(|entry| entry.superseded_by.as_ref().map(BlockId::as_str))
        .collect();
    assert_eq!(superseded, vec![Some("e2"), Some("a2")]);
}

#[test]
fn the_least_important_picks_shrink_then_go_to_fit_the_budget() {
    let session = retry_session();
    let history = session.history();
    let picks = [pick(&session, "t3", Presentation::Full)];
    let roomy = plan_view(&history, &picks, limits(1));

    let tight = plan_view(
        &history,
        &picks,
        ViewLimits {
            budget_tokens: roomy.tokens - 1,
            ..limits(1)
        },
    );
    assert_eq!(
        shown(&tight)[2],
        ("t3", Presentation::Summary, Reason::Selected)
    );
    assert_eq!((tight.shrunk, tight.dropped), (1, 0));

    let tiny = plan_view(
        &history,
        &picks,
        ViewLimits {
            budget_tokens: 1,
            ..limits(1)
        },
    );
    assert_eq!(
        shown(&tiny),
        vec![
            ("u1", Presentation::Full, Reason::TaskStart),
            ("u4", Presentation::Full, Reason::Recent),
        ]
    );
    assert_eq!((tiny.shrunk, tiny.dropped), (1, 2));
    assert_eq!(tiny.map[2], "turn 3: run the tests → retry_limit fails");
}

#[test]
fn rendering_puts_the_map_and_ledger_first_and_keeps_full_blocks_as_recorded() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(
        &history,
        &[pick(&session, "t3", Presentation::Full)],
        limits(1),
    );
    let items: Vec<String> = render_view(&history, &view, RenderOptions::default())
        .iter()
        .map(text)
        .collect();

    assert_eq!(items.len(), 6);
    assert!(items[0].starts_with("developer: Parts of this conversation's history are omitted"));
    assert!(items[0].ends_with(
        "## Earlier turns\n\
         turn 1: add retries to the HTTP client → plan: exponential backoff [1 shown below]\n\
         turn 2: use a fixed delay instead → switch to a fixed delay [1 shown below]\n\
         turn 3: run the tests → retry_limit fails [1 shown below]\n\n\
         ## Constraints and decisions\n\
         - (user constraint, turn 1) Do not change the public API\n\
         - (decision, turn 2) Retry with a fixed 100 ms delay"
    ));
    assert_eq!(
        items[1..],
        [
            "user: Add retries to the HTTP client. Do not change the public API.".to_string(),
            "developer: Recalled from earlier turns:\n\n[turn 2 · tool call] edit client.rs\n\
             summary of edit client.rs"
                .to_string(),
            "call c3".to_string(),
            "output c3".to_string(),
            "user: Why did the test fail?".to_string(),
        ]
    );
}

#[test]
fn excerpts_and_outdated_blocks_render_as_text() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(
        &history,
        &[
            pick(
                &session,
                "t3",
                Presentation::Excerpt {
                    lines: vec![LineRange { from: 1, to: 1 }, LineRange { from: 7, to: 8 }],
                },
            ),
            pick(&session, "r1", Presentation::Full),
        ],
        limits(1),
    );
    let items: Vec<String> = render_view(&history, &view, RenderOptions::default())
        .iter()
        .map(text)
        .collect();
    assert_eq!(
        items[2],
        "developer: Recalled from earlier turns:\n\n\
         [turn 1 · tool call] $ cat src/client.rs (outdated: replaced in turn 2 by \"edit client.rs\")\n\
         summary of $ cat src/client.rs\n\n\
         [turn 2 · tool call] edit client.rs\nsummary of edit client.rs\n\n\
         [turn 3 · tool call] $ cargo test (excerpt)\n\
         [tool call exec_command]\n[...]\ntest retry_limit ... FAILED\nexpected 3 attempts, got 4"
    );
}

#[test]
fn reasoning_stays_only_in_recent_turns_and_only_when_asked() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(&history, &[], limits(4));
    let render = |keep_recent_reasoning| -> Vec<String> {
        render_view(
            &history,
            &view,
            RenderOptions {
                keep_recent_reasoning,
                tools_hint: false,
            },
        )
        .iter()
        .map(text)
        .collect()
    };
    assert!(!render(false).contains(&"reasoning".to_string()));
    assert!(render(true).contains(&"reasoning".to_string()));
}

#[test]
fn the_tools_hint_follows_the_preamble() {
    let session = retry_session();
    let history = session.history();
    let view = plan_view(&history, &[], limits(1));
    let items = render_view(
        &history,
        &view,
        RenderOptions {
            keep_recent_reasoning: false,
            tools_hint: true,
        },
    );
    assert!(text(&items[0]).ends_with(
        "To read omitted history, find blocks with `context_search` and read a block or a whole \
         turn with `context_expand`."
    ));
}

#[test]
fn referenced_blocks_are_shown_only_when_picked_and_as_text() {
    let mut session = retry_session();
    let u4 = session.index("u4");
    session.insert_referenced(
        u4,
        "jitter",
        crate::test_support::card("j1", BlockKind::AssistantMessage, None, "jitter works"),
        vec![crate::test_support::assistant(
            "j1",
            "Full jitter passed every test.",
        )],
    );
    let history = session.history();

    let unpicked = plan_view(&history, &[], limits(1));
    assert!(!unpicked.shows(session.index("j1")));

    let view = plan_view(
        &history,
        &[pick(&session, "j1", Presentation::Full)],
        limits(1),
    );
    assert_eq!(
        shown(&view)[1..],
        [
            ("u4", Presentation::Full, Reason::Recent),
            ("j1", Presentation::Full, Reason::Selected),
        ]
    );
    let items: Vec<String> = render_view(&history, &view, RenderOptions::default())
        .iter()
        .map(text)
        .collect();
    assert_eq!(
        items[2..],
        [
            "user: Why did the test fail?".to_string(),
            "developer: Recalled from earlier turns:\n\n[branch jitter · assistant] jitter \
             works\n[assistant]\nFull jitter passed every test."
                .to_string(),
        ]
    );
}

#[test]
fn blocks_whose_files_changed_are_summarized_and_marked() {
    let session = retry_session();
    let mut history_blocks = session.history().blocks().to_vec();
    let r1 = session.index("r1");
    history_blocks[r1].changed = true;
    let history = crate::history::History::new(history_blocks);
    let view = plan_view(
        &history,
        &[pick(&session, "r1", Presentation::Full)],
        limits(1),
    );
    let entry = view
        .entries
        .iter()
        .find(|entry| entry.index == r1)
        .unwrap_or_else(|| panic!("r1 not shown"));
    assert_eq!(
        (entry.presentation.clone(), entry.file_changed),
        (Presentation::Summary, true)
    );
    assert!(entry_text(&history, entry).contains("(outdated: a file it read has changed since)"));
}

#[test]
fn long_maps_sum_up_episodes_the_view_does_not_show() {
    use crate::BlockKind;
    use crate::episode::EpisodeCard;
    use crate::test_support::card;

    let mut session = Fixture::default();
    for turn in 1..=20 {
        session.push(
            card(
                &format!("u{turn}"),
                BlockKind::UserMessage,
                None,
                &format!("request {turn}"),
            ),
            Vec::new(),
        );
        session.push(
            card(
                &format!("a{turn}"),
                BlockKind::AssistantMessage,
                None,
                &format!("result {turn}"),
            ),
            Vec::new(),
        );
    }
    let episode = |first: usize, last: usize, title: &str| EpisodeCard {
        first_block: BlockId::from_stored(format!("u{first}")),
        last_block: BlockId::from_stored(format!("u{last}")),
        turns: last - first + 1,
        indexer: "test".to_string(),
        title: title.to_string(),
        summary: format!("{title} done"),
    };
    let episodes = [episode(6, 10, "retries"), episode(11, 16, "docs")];
    let history = session.history().with_episodes(&episodes);
    let limits = ViewLimits {
        recent_turns: 1,
        max_map_turns: 10,
        ..ViewLimits::default()
    };

    // Turns 1 to 19 are mapped; the two episodes leave 10 lines.
    let view = plan_view(&history, &[], limits);
    assert_eq!(
        view.map[4..8],
        [
            "turn 5: request 5 → result 5".to_string(),
            "turns 6–10: retries — retries done".to_string(),
            "turns 11–16: docs — docs done".to_string(),
            "turn 17: request 17 → result 17".to_string(),
        ]
    );
    assert_eq!(view.map.len(), 10);

    // A turn whose block the view shows is listed on its own, and its episode is not summed up.
    let picked = plan_view(
        &history,
        &[pick(&session, "a8", Presentation::Full)],
        limits,
    );
    assert!(picked.map.iter().all(|line| !line.contains("retries")));
    assert!(
        picked
            .map
            .iter()
            .any(|line| line.starts_with("turns 11–16: docs"))
    );
    // Still too long for the map, so the middle is cut, counting the turns left out.
    assert_eq!(picked.map.len(), 11);
    assert_eq!(picked.map[5], "… 4 turns not listed …");
}
