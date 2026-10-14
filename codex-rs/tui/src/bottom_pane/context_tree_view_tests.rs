use codex_app_server_protocol::ContextGraphBlock;
use codex_app_server_protocol::ContextGraphMerge;
use codex_app_server_protocol::ContextGraphMode;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

use super::*;

fn branch(id: &str, name: &str, depth: u32, current: bool) -> ContextGraphBranch {
    ContextGraphBranch {
        branch_id: id.to_string(),
        name: name.to_string(),
        thread_id: Some(format!("00000000-0000-0000-0000-00000000000{}", &id[1..])),
        parent_branch_id: (depth > 0).then(|| "b1".to_string()),
        depth,
        status: ContextGraphBranchStatus::Active,
        turns: 1,
        last_request: None,
        summary: None,
        purpose: None,
        current,
    }
}

fn block(id: &str, kind: &str, title: &str, tokens: u32) -> ContextGraphBlock {
    ContextGraphBlock {
        block_id: id.to_string(),
        kind: kind.to_string(),
        title: title.to_string(),
        tokens,
    }
}

fn turns() -> Vec<ContextGraphTurn> {
    vec![
        ContextGraphTurn {
            number: 1,
            turn_id: Some("turn-1".to_string()),
            inherited: true,
            title: "add retries".to_string(),
            blocks: vec![block("u1", "user_message", "add retries", 20)],
        },
        ContextGraphTurn {
            number: 2,
            turn_id: Some("turn-2".to_string()),
            inherited: false,
            title: "try jitter".to_string(),
            blocks: vec![
                block("u2", "user_message", "try jitter", 30),
                block("t2", "tool_exchange", "$ cargo test jitter", 1_800),
                block("a2", "assistant_message", "jitter works", 120),
            ],
        },
    ]
}

fn view() -> (ContextTreeView, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    let mut archived = branch("b3", "old-try", 1, false);
    archived.status = ContextGraphBranchStatus::Archived;
    let graph = ContextGraphReadResponse {
        graph_id: Some("graph".to_string()),
        branches: vec![
            branch("b1", "main", 0, true),
            branch("b2", "jitter", 1, false),
            archived,
        ],
        merges: vec![ContextGraphMerge {
            target_branch_id: "b1".to_string(),
            source_branch_id: "b2".to_string(),
            mode: ContextGraphMergeMode::Reference,
        }],
        mode: Some(ContextGraphMode::Recall),
    };
    let view = ContextTreeView::new(
        ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .unwrap_or_else(|err| panic!("thread id: {err}")),
        &graph,
        vec!["main".into(), "├ jitter".into(), "└ old-try".into()],
        vec![
            "3 turns".into(),
            "1 turn · referenced by main".into(),
            "1 turn · archived".into(),
        ],
        AppEventSender::new(tx),
    );
    (view, rx)
}

fn render(view: &ContextTreeView, width: u16) -> String {
    let height = view.desired_height(width);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|col| buf[(col, row)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(view: &mut ContextTreeView, code: KeyCode) {
    view.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
}

fn events(rx: &mut UnboundedReceiver<AppEvent>) -> Vec<AppEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

/// Opens the `jitter` branch and its own turn.
fn expanded() -> (ContextTreeView, UnboundedReceiver<AppEvent>) {
    let (mut view, mut rx) = view();
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Right);
    let loads: Vec<String> = events(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            AppEvent::ContextTreeLoadBranch { branch_id, .. } => Some(branch_id),
            _ => None,
        })
        .collect();
    assert_eq!(loads, vec!["b2".to_string()]);
    assert!(view.update_context_tree(&ContextTreeUpdate::Turns {
        branch_id: "b2".to_string(),
        result: Ok(turns()),
    }));
    // Branch, inherited note, then the branch's own turn.
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Right);
    (view, rx)
}

#[test]
fn branches_expand_to_their_turns_and_blocks() {
    let (view, _rx) = expanded();
    assert_snapshot!("context_tree_view_expanded", render(&view, 100));
}

#[test]
fn reading_forking_and_merging_start_from_the_selected_row() {
    let (mut view, mut rx) = expanded();
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Char('p'));
    match events(&mut rx).as_slice() {
        [
            AppEvent::ContextTreePreview {
                title, block_ids, ..
            },
        ] => {
            assert_eq!(title, "jitter · turn 2 · $ cargo test jitter");
            assert_eq!(block_ids, &vec!["t2".to_string()]);
        }
        other => panic!("unexpected events: {other:?}"),
    }

    press(&mut view, KeyCode::Char('f'));
    match events(&mut rx).as_slice() {
        [
            AppEvent::ContextTreeFork {
                source_thread_id,
                turn,
            },
        ] => {
            assert_eq!(*turn, 2);
            assert_eq!(
                source_thread_id.to_string(),
                "00000000-0000-0000-0000-000000000002"
            );
        }
        other => panic!("unexpected events: {other:?}"),
    }
    assert!(view.is_complete());

    let (mut view, mut rx) = expanded();
    press(&mut view, KeyCode::Char('r'));
    match events(&mut rx).as_slice() {
        [AppEvent::MergeCurrentSession { args, .. }] => {
            assert_eq!(args, &vec!["jitter".to_string(), "--reference".to_string()]);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn archiving_toggles_and_reopens_the_tree() {
    let (mut view, mut rx) = view();
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Char('a'));
    match events(&mut rx).as_slice() {
        [
            AppEvent::ContextBranchUpdate {
                branch,
                archived,
                reopen_tree,
                ..
            },
        ] => {
            assert_eq!(branch.as_deref(), Some("old-try"));
            assert_eq!(*archived, Some(false));
            assert!(*reopen_tree);
        }
        other => panic!("unexpected events: {other:?}"),
    }
    // The current branch cannot be archived from the tree.
    let (mut view, mut rx) = self::view();
    press(&mut view, KeyCode::Char('a'));
    assert!(events(&mut rx).is_empty());
    assert!(!view.is_complete());
}

#[test]
fn searches_show_hits_from_every_branch() {
    let (mut view, mut rx) = view();
    press(&mut view, KeyCode::Char('/'));
    for ch in "jitter".chars() {
        press(&mut view, KeyCode::Char(ch));
    }
    press(&mut view, KeyCode::Enter);
    match events(&mut rx).as_slice() {
        [AppEvent::ContextTreeSearch { query, .. }] => assert_eq!(query, "jitter"),
        other => panic!("unexpected events: {other:?}"),
    }
    // Results of another search are ignored.
    assert!(!view.update_context_tree(&ContextTreeUpdate::Search {
        query: "other".to_string(),
        result: Ok(Vec::new()),
    }));
    assert!(view.update_context_tree(&ContextTreeUpdate::Search {
        query: "jitter".to_string(),
        result: Ok(vec![ContextGraphSearchHit {
            branch_id: "b2".to_string(),
            branch_name: "jitter".to_string(),
            turn: 2,
            block: block("t2", "tool_exchange", "$ cargo test jitter", 1_800),
            summary: "the jitter tests pass".to_string(),
        }]),
    }));
    assert_snapshot!("context_tree_view_search", render(&view, 100));

    press(&mut view, KeyCode::Enter);
    match events(&mut rx).as_slice() {
        [AppEvent::ContextTreePreview { block_ids, .. }] => {
            assert_eq!(block_ids, &vec!["t2".to_string()]);
        }
        other => panic!("unexpected events: {other:?}"),
    }
    // Esc goes back to the tree, then closes it.
    press(&mut view, KeyCode::Esc);
    assert!(!view.is_complete());
    press(&mut view, KeyCode::Esc);
    assert!(view.is_complete());
}
