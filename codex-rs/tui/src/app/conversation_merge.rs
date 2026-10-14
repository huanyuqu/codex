//! `/merge` and `/tree` for the interactive CLI: both work on the context graph of the current
//! chat, through the app server's `contextGraph/*` requests, so they work with remote servers
//! too. Saved chats outside the conversation can be merged by ID; the merge picker lists them
//! after the conversation's branches.

use super::*;
use crate::bottom_pane::MultiSelectItem;
use codex_app_server_client::AppServerRequestHandle;
use color_eyre::eyre::eyre;
use serde_json::Value;
use serde_json::json;

const GRAPH_OFF: &str = "Branches live in the context graph; turn it on with \
                         `context_graph = true` under [features] in config.toml.";

impl App {
    pub(super) async fn merge_current_session(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        args: Vec<String>,
    ) -> Result<AppRunControl> {
        if self.chat_widget.thread_id() != Some(primary_thread_id)
            || self.chat_widget.is_user_turn_pending_or_running()
            || self.chat_widget.is_external_writer_view()
            || self
                .pending_server_profiles
                .contains_key(&primary_thread_id)
        {
            self.chat_widget.add_error_message(
                "Wait for the current chat to be idle, then run /merge again.".into(),
            );
            return Ok(AppRunControl::Continue);
        }
        if !self.context_graph_branches_enabled() {
            self.chat_widget.add_error_message(GRAPH_OFF.into());
            return Ok(AppRunControl::Continue);
        }
        Box::pin(self.merge_graph_branches(tui, app_server, primary_thread_id, args)).await
    }

    pub(super) async fn open_context_tree(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        current_thread_id: ThreadId,
        target: Option<String>,
    ) -> Result<AppRunControl> {
        if self.chat_widget.thread_id() != Some(current_thread_id)
            || self.chat_widget.is_user_turn_pending_or_running()
        {
            self.chat_widget.add_error_message(
                "Wait for the current chat to be idle, then run /tree again.".into(),
            );
            return Ok(AppRunControl::Continue);
        }
        if !self.context_graph_branches_enabled() {
            self.chat_widget.add_error_message(GRAPH_OFF.into());
            return Ok(AppRunControl::Continue);
        }
        Box::pin(self.open_graph_tree(tui, app_server, current_thread_id, target)).await
    }

    pub(super) async fn switch_context_branch(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        current_thread_id: ThreadId,
        target_thread_id: ThreadId,
    ) -> Result<AppRunControl> {
        if self.chat_widget.thread_id() != Some(current_thread_id)
            || self.chat_widget.is_user_turn_pending_or_running()
            || self
                .pending_server_profiles
                .contains_key(&current_thread_id)
        {
            self.chat_widget
                .add_error_message("The current chat changed or is busy. Run /tree again.".into());
            return Ok(AppRunControl::Continue);
        }
        if current_thread_id == target_thread_id {
            return Ok(AppRunControl::Continue);
        }
        match crate::lookup_session_target_with_app_server(
            app_server,
            &self.config,
            &target_thread_id.to_string(),
        )
        .await
        {
            Ok(Some(target)) => self.resume_target_session(tui, app_server, target).await,
            Ok(None) => {
                self.chat_widget.add_error_message(
                    "The selected branch is no longer available. Run /tree to refresh.".into(),
                );
                Ok(AppRunControl::Continue)
            }
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not open branch: {error:#}"));
                Ok(AppRunControl::Continue)
            }
        }
    }
}

/// Saved, idle chats other than `primary_thread_id` and those in `exclude`, for the merge picker.
pub(super) async fn merge_picker_items(
    handle: AppServerRequestHandle,
    primary_thread_id: ThreadId,
    exclude: &HashSet<String>,
) -> Result<Vec<MultiSelectItem>> {
    let mut items = Vec::new();
    let mut cursor = Value::Null;
    let mut seen_cursors = HashSet::new();
    loop {
        let request: ClientRequest = serde_json::from_value(json!({
            "id": format!("merge-picker-{}", Uuid::new_v4()),
            "method": "thread/list",
            "params": {
                "limit": 100, "cursor": cursor, "sortKey": "updated_at", "modelProviders": [],
            },
        }))?;
        let response = handle
            .request(request)
            .await?
            .map_err(|error| eyre!(error.message))?;
        for thread in response["data"]
            .as_array()
            .ok_or_else(|| eyre!("Invalid thread list"))?
        {
            let Some(id) = thread["id"].as_str() else {
                continue;
            };
            if id == primary_thread_id.to_string()
                || exclude.contains(id)
                || thread["ephemeral"].as_bool() == Some(true)
                || matches!(
                    thread["status"]["type"].as_str(),
                    Some("active" | "systemError")
                )
                || thread["path"].as_str().is_none()
            {
                continue;
            }
            let name = thread["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .or_else(|| {
                    thread["preview"]
                        .as_str()
                        .filter(|preview| !preview.is_empty())
                })
                .unwrap_or(id)
                .replace(['\n', '\r'], " ");
            items.push(MultiSelectItem {
                id: id.into(),
                name,
                description: Some(format!(
                    "saved chat {id} · {}",
                    thread["cwd"].as_str().unwrap_or("")
                )),
                orderable: false,
                ..Default::default()
            });
        }
        cursor = response["nextCursor"].clone();
        if cursor.is_null() {
            return Ok(items);
        }
        if !seen_cursors.insert(cursor.to_string()) {
            color_eyre::eyre::bail!("Repeated conversation list cursor");
        }
    }
}
