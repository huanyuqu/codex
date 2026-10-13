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
