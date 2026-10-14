//! Commands the model left running in the background. When one exits, the model gets a message
//! with its exit code and the end of its output, so it does not have to poll the command and its
//! turn does not wait on it. A session that is idle starts a turn for the message unless
//! `background_terminal_wake` is off, in which case the message waits for the next turn.

use std::sync::Arc;
use std::time::Duration;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

use super::session::Session;

/// Characters of a finished command's output the message keeps, from its end.
const NOTICE_OUTPUT_CHARS: usize = 6_000;

/// A background command that exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundCommandExit {
    pub(crate) process_id: i32,
    pub(crate) command: String,
    pub(crate) exit_code: Option<i32>,
    /// Why the process failed, when it did not exit on its own.
    pub(crate) failure: Option<String>,
    pub(crate) duration: Duration,
    pub(crate) output: String,
}

/// Tells the model that a command it left running exited: in the running turn, or in a turn
/// started for it, or, without wake, in the history the next turn starts from.
pub(crate) async fn notify_command_exit(session: &Arc<Session>, exit: BackgroundCommandExit) {
    let notice = notice_item(&exit);
    if !session.get_config().await.background_terminal_wake {
        session
            .inject_no_new_turn(vec![notice], /*current_turn_context*/ None)
            .await;
        return;
    }
    let Err(notices) = session.inject_if_running(vec![notice]).await else {
        return;
    };
    for notice in notices {
        session.input_queue.enqueue_background_notice(notice).await;
    }
    session.maybe_start_turn_for_pending_work().await;
}

fn notice_item(exit: &BackgroundCommandExit) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "developer".to_string(),
        content: vec![ContentItem::InputText {
            text: notice_text(exit),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// The message about an exited command: what ran, how it ended, and the end of its output.
pub(crate) fn notice_text(exit: &BackgroundCommandExit) -> String {
    let ended = match (&exit.failure, exit.exit_code) {
        (Some(failure), _) => format!("failed: {failure}"),
        (None, Some(code)) => format!("exited with code {code}"),
        (None, None) => "exited".to_string(),
    };
    let output = exit.output.trim_end();
    let chars = output.chars().count();
    let output = if chars > NOTICE_OUTPUT_CHARS {
        let tail: String = output.chars().skip(chars - NOTICE_OUTPUT_CHARS).collect();
        format!(
            "[... {} earlier characters omitted ...]\n{tail}",
            chars - NOTICE_OUTPUT_CHARS
        )
    } else if output.is_empty() {
        "(no output)".to_string()
    } else {
        output.to_string()
    };
    format!(
        "<background_command_exit session_id=\"{}\">\nThe command `{}` you left running in the \
         background {ended} after {:.1}s. Its session is closed. The end of its output:\n{output}\n\
         </background_command_exit>",
        exit.process_id,
        exit.command.trim(),
        exit.duration.as_secs_f64()
    )
}

#[cfg(test)]
#[path = "background_commands_tests.rs"]
mod tests;
