//! Recall for the request being written: when the user pauses typing, the app asks recall to
//! start choosing context for the draft, so the turn it starts need not wait for recall.

use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;

use super::ChatWidget;

/// How long typing must pause before the draft is recalled for.
pub(crate) const DRAFT_PAUSE: Duration = Duration::from_millis(1_200);
/// Drafts shorter than this, in characters, are not recalled for.
const MIN_DRAFT_CHARS: usize = 8;
/// Characters a draft that only grew or shrank must change by since the last one recalled for.
const MIN_DRAFT_CHANGE: usize = 12;

/// The composer's draft and the last one recalled for.
#[derive(Debug, Default)]
pub(crate) struct DraftRecall {
    text: String,
    /// When the draft last changed, while it waits for a pause.
    changed_at: Option<Instant>,
    /// The last draft recalled for, until the composer empties.
    recalled: Option<String>,
}

impl DraftRecall {
    /// Notes the composer's text at `now`; returns whether a pause now has to be waited for.
    pub(crate) fn note(&mut self, text: &str, now: Instant) -> bool {
        if text == self.text {
            return false;
        }
        self.text = text.to_string();
        if text.trim().is_empty() {
            // Sent or cleared: the next draft starts afresh.
            self.changed_at = None;
            self.recalled = None;
            return false;
        }
        self.changed_at = Some(now);
        true
    }

    /// How long until typing has paused, while a changed draft waits for it.
    pub(crate) fn pause_ends_in(&self, now: Instant) -> Option<Duration> {
        let changed_at = self.changed_at?;
        Some(DRAFT_PAUSE.saturating_sub(now.saturating_duration_since(changed_at)))
    }

    /// The draft to recall for, once typing has paused and the draft changed enough since the
    /// last one recalled for.
    pub(crate) fn take_paused(&mut self, now: Instant) -> Option<String> {
        if !self.pause_ends_in(now)?.is_zero() {
            return None;
        }
        self.changed_at = None;
        let draft = self.text.trim();
        if !worth_recalling(draft, self.recalled.as_deref()) {
            return None;
        }
        self.recalled = Some(draft.to_string());
        Some(draft.to_string())
    }
}

impl ChatWidget {
    /// Notes the composer's text after an edit, and wakes when typing would have paused.
    pub(super) fn note_composer_draft(&mut self) {
        let now = Instant::now();
        if self
            .draft_recall
            .note(&self.bottom_pane.composer_text(), now)
        {
            self.frame_requester.schedule_frame_in(DRAFT_PAUSE);
        }
    }

    /// The thread and draft to recall for, once typing has paused on a draft worth it.
    pub(crate) fn take_paused_draft(&mut self) -> Option<(ThreadId, String)> {
        let thread_id = self.thread_id?;
        let now = Instant::now();
        match self.draft_recall.pause_ends_in(now) {
            Some(remaining) if !remaining.is_zero() => {
                // Woken early by another frame: wake again when the pause ends.
                self.frame_requester.schedule_frame_in(remaining);
                None
            }
            _ => self
                .draft_recall
                .take_paused(now)
                .map(|draft| (thread_id, draft)),
        }
    }
}

/// Whether `draft` is a request worth recalling for, given the last draft recalled for.
fn worth_recalling(draft: &str, recalled: Option<&str>) -> bool {
    // Slash commands and shell commands are not requests to the model.
    if draft.starts_with('/') || draft.starts_with('!') || draft.chars().count() < MIN_DRAFT_CHARS {
        return false;
    }
    match recalled {
        None => true,
        Some(recalled) if draft.starts_with(recalled) || recalled.starts_with(draft) => {
            draft.chars().count().abs_diff(recalled.chars().count()) >= MIN_DRAFT_CHANGE
        }
        // An edit inside the draft changes what it asks.
        Some(recalled) => draft != recalled,
    }
}

#[cfg(test)]
#[path = "draft_recall_tests.rs"]
mod tests;
