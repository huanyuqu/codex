use std::time::Duration;
use std::time::Instant;

use pretty_assertions::assert_eq;

use super::*;

#[test]
fn a_draft_is_recalled_for_once_typing_pauses() {
    let start = Instant::now();
    let mut draft = DraftRecall::default();
    assert!(draft.note("Why does the retry test", start));
    assert_eq!(draft.pause_ends_in(start), Some(DRAFT_PAUSE));
    // Still typing.
    assert_eq!(draft.take_paused(start + Duration::from_millis(500)), None);
    assert!(draft.note(
        "Why does the retry test fail?",
        start + Duration::from_millis(600)
    ));
    assert_eq!(
        draft.take_paused(start + Duration::from_millis(1_500)),
        None
    );
    let paused = start + Duration::from_millis(600) + DRAFT_PAUSE;
    assert_eq!(
        draft.take_paused(paused),
        Some("Why does the retry test fail?".to_string())
    );
    // Recalled for once.
    assert_eq!(draft.pause_ends_in(paused), None);
    assert_eq!(draft.take_paused(paused + DRAFT_PAUSE), None);
}

#[test]
fn small_additions_short_drafts_and_commands_are_not_recalled_for() {
    let start = Instant::now();
    let later = |step: u64| start + DRAFT_PAUSE * u32::try_from(step).unwrap_or(1);
    let mut draft = DraftRecall::default();
    draft.note("Why does the retry test fail?", start);
    assert!(draft.take_paused(later(1)).is_some());
    // A question mark more is the same request.
    draft.note("Why does the retry test fail??", later(1));
    assert_eq!(draft.take_paused(later(2)), None);
    // A longer addition asks more.
    draft.note(
        "Why does the retry test fail?? Look at the backoff cap too.",
        later(2),
    );
    assert!(draft.take_paused(later(3)).is_some());
    // An edit inside the draft changes the request.
    draft.note(
        "Why does the timeout test fail?? Look at the backoff cap too.",
        later(3),
    );
    assert!(draft.take_paused(later(4)).is_some());

    for text in ["/compact now please", "!cargo test --all", "fix it"] {
        let mut draft = DraftRecall::default();
        draft.note(text, start);
        assert_eq!(draft.take_paused(later(1)), None, "{text}");
    }
}

#[test]
fn an_emptied_composer_starts_the_next_draft_afresh() {
    let start = Instant::now();
    let mut draft = DraftRecall::default();
    draft.note("Why does the retry test fail?", start);
    assert!(draft.take_paused(start + DRAFT_PAUSE).is_some());
    // Sent: the same words again are a new request.
    assert!(!draft.note("", start + DRAFT_PAUSE));
    draft.note("Why does the retry test fail?", start + DRAFT_PAUSE * 2);
    assert!(draft.take_paused(start + DRAFT_PAUSE * 3).is_some());
}
