//! Conversation graph merge and branch navigation for the interactive CLI.
//!
//! The Node helper shares the existing graph engine with `codex merge`. Its JSON-RPC requests
//! use this TUI's app-server connection, preserving ownership of the current thread. Only
//! helper-owned threads receive analysis notifications; other events retain normal TUI routing.

use super::app_server_event_targets::ServerNotificationThreadTarget;
use super::app_server_event_targets::server_notification_thread_target;
use super::app_server_event_targets::server_request_thread_id;
use super::*;
use crate::bottom_pane::MultiSelectItem;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use color_eyre::eyre::eyre;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::Lines;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::task::JoinSet;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum MergeMessage {
    Rpc { message: Value },
    Progress { text: String },
    Result { result: Value },
    Error { message: String },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContextTreeRow {
    id: String,
    name: String,
    description: String,
    search: String,
    current: bool,
    disabled_reason: Option<String>,
}

#[derive(Deserialize)]
struct ContextTreeResult {
    rows: Vec<ContextTreeRow>,
    warnings: Vec<String>,
}

struct MergeThread {
    ephemeral: bool,
    turn_id: Option<String>,
}

struct MergeRpc {
    request: Value,
    response: Value,
}

struct MergeBridge {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    requests: JoinSet<MergeRpc>,
    threads: HashMap<ThreadId, MergeThread>,
}

impl MergeBridge {
    fn start(config: &Config) -> Result<Self> {
        let script = std::env::var_os("CODEX_CONTEXT_MERGE_SCRIPT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../codex-cli/bin/merge-ui.js")
            });
        if !script.is_file() {
            color_eyre::eyre::bail!(
                "Conversation graph commands need the context-graph launcher. Start this checkout with ./codex."
            );
        }
        let node = std::env::var_os("CODEX_CONTEXT_MERGE_NODE").unwrap_or_else(|| "node".into());
        let mut child = Command::new(node)
            .arg(script)
            .env("CODEX_HOME", config.codex_home.as_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .wrap_err(
                "Could not start conversation graph helper; Node.js 22 or newer is required",
            )?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| eyre!("Missing merge input"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("Missing merge output"))?;
        Ok(Self {
            child,
            input,
            output: BufReader::new(output).lines(),
            requests: JoinSet::new(),
            threads: HashMap::new(),
        })
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        self.input.write_all(&bytes).await?;
        self.input.flush().await?;
        Ok(())
    }

    fn request(&mut self, handle: AppServerRequestHandle, message: Value) {
        self.requests.spawn(async move {
            let id = message["id"].clone();
            let response = match serde_json::from_value::<ClientRequest>(message.clone()) {
                Ok(request) => match tokio::time::timeout(Duration::from_secs(35), handle.request(request)).await {
                    Ok(Ok(Ok(result))) => json!({"id": id, "result": result}),
                    Ok(Ok(Err(error))) => json!({"id": id, "error": error}),
                    Ok(Err(error)) => json!({"id": id, "error": {"code": -32000, "message": error.to_string()}}),
                    Err(_) => json!({"id": id, "error": {"code": -32000, "message": "Conversation merge request timed out"}}),
                },
                Err(error) => json!({"id": id, "error": {"code": -32602, "message": error.to_string()}}),
            };
            MergeRpc { request: message, response }
        });
    }

    fn record_rpc(&mut self, rpc: &MergeRpc) {
        match rpc.request["method"].as_str() {
            Some("thread/start" | "thread/fork") => {
                if let Some(id) = rpc.response["result"]["thread"]["id"]
                    .as_str()
                    .and_then(|id| ThreadId::from_string(id).ok())
                {
                    self.threads.insert(
                        id,
                        MergeThread {
                            ephemeral: rpc.request["params"]["ephemeral"]
                                .as_bool()
                                .unwrap_or(false),
                            turn_id: None,
                        },
                    );
                }
            }
            Some("turn/start") => {
                if let Some(thread) = rpc.request["params"]["threadId"]
                    .as_str()
                    .and_then(|id| ThreadId::from_string(id).ok())
                    .and_then(|id| self.threads.get_mut(&id))
                {
                    thread.turn_id = rpc.response["result"]["turn"]["id"]
                        .as_str()
                        .map(str::to_owned);
                }
            }
            _ => {}
        }
    }

    // The helper normally unsubscribes/archives itself. Repeat cleanup on cancellation, a broken
    // pipe, or a helper crash so hidden analyses cannot retain writers after returning to the chat.
    async fn finish(&mut self, handle: AppServerRequestHandle, success: bool) {
        let _ = self.child.kill().await;
        let cleanup = async {
            while let Some(rpc) = self.requests.join_next().await {
                if let Ok(rpc) = rpc {
                    self.record_rpc(&rpc);
                }
            }
            for (id, thread) in &self.threads {
                if let Some(turn_id) = &thread.turn_id {
                    let request: ClientRequest = serde_json::from_value(json!({
                        "id": format!("merge-cleanup-{}", Uuid::new_v4()),
                        "method": "turn/interrupt",
                        "params": {"threadId": id.to_string(), "turnId": turn_id},
                    }))?;
                    let _ = handle.request(request).await;
                }
                let method = if !success && !thread.ephemeral {
                    "thread/archive"
                } else {
                    "thread/unsubscribe"
                };
                let request: ClientRequest = serde_json::from_value(json!({
                    "id": format!("merge-cleanup-{}", Uuid::new_v4()),
                    "method": method,
                    "params": {"threadId": id.to_string()},
                }))?;
                let _ = handle.request(request).await;
            }
            Ok::<_, serde_json::Error>(())
        };
        if tokio::time::timeout(Duration::from_secs(10), cleanup)
            .await
            .is_err()
        {
            tracing::warn!("conversation merge cleanup timed out");
        }
    }
}

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
        if app_server.uses_remote_workspace() {
            self.chat_widget.add_error_message(
                "/merge currently needs local saved chats. Start a local CLI session to merge them.".into(),
            );
            return Ok(AppRunControl::Continue);
        }
        if self.windows_sandbox_blocks_thread_switch()
            || self.reject_pending_permission_root_switch()
        {
            return Ok(AppRunControl::Continue);
        }
        if args.is_empty() {
            match merge_picker_items(app_server.request_handle(), primary_thread_id).await {
                Ok(items) if !items.is_empty() => {
                    self.chat_widget.show_conversation_merge_picker(
                        primary_thread_id,
                        items,
                        Vec::new(),
                    );
                }
                Ok(_) => self
                    .chat_widget
                    .add_info_message("No other saved, idle chats to merge.".into(), None),
                Err(error) => self
                    .chat_widget
                    .add_error_message(format!("Could not list merge branches: {error:#}")),
            }
            return Ok(AppRunControl::Continue);
        }
        let mut bridge = match MergeBridge::start(&self.config) {
            Ok(bridge) => bridge,
            Err(error) => {
                self.chat_widget.add_error_message(format!("{error:#}"));
                return Ok(AppRunControl::Continue);
            }
        };
        self.chat_widget
            .set_conversation_merge_status(Some("Esc cancels.".into()));
        let outcome = self
            .run_context_operation(
                tui,
                app_server,
                &mut bridge,
                json!({
                    "type": "start", "primaryThreadId": primary_thread_id.to_string(),
                    "args": args, "model": self.chat_widget.current_model(),
                }),
                "Merging conversations",
            )
            .await;
        self.chat_widget.set_conversation_merge_status(None);
        bridge
            .finish(app_server.request_handle(), matches!(&outcome, Ok(Some(_))))
            .await;
        match outcome {
            Ok(Some(result)) => {
                if let Some(help) = result["help"].as_str() {
                    self.chat_widget.add_info_message(help.into(), None);
                } else if result["selectBranches"].as_bool() == Some(true) {
                    let args = serde_json::from_value::<Vec<String>>(result["args"].clone())?;
                    match merge_picker_items(app_server.request_handle(), primary_thread_id).await {
                        Ok(items) if !items.is_empty() => self
                            .chat_widget
                            .show_conversation_merge_picker(primary_thread_id, items, args),
                        Ok(_) => self
                            .chat_widget
                            .add_info_message("No other saved, idle chats to merge.".into(), None),
                        Err(error) => self
                            .chat_widget
                            .add_error_message(format!("Could not list merge branches: {error:#}")),
                    }
                } else if result["dryRun"].as_bool() == Some(true) {
                    self.chat_widget.add_info_message(
                        format!("Merge preview:\n{}", serde_json::to_string_pretty(&result)?),
                        None,
                    );
                } else if let Some(thread_id) = result["threadId"].as_str() {
                    let target = crate::lookup_session_target_with_app_server(
                        app_server,
                        &self.config,
                        thread_id,
                    )
                    .await;
                    if let Ok(Some(target)) = target {
                        let target_id = target.thread_id;
                        let control = self.resume_target_session(tui, app_server, target).await?;
                        if self.chat_widget.thread_id() == Some(target_id) {
                            self.chat_widget.add_info_message(
                                "Branches merged. You can continue here.".into(),
                                None,
                            );
                        } else {
                            self.chat_widget.add_info_message(
                                format!("Merge saved as {thread_id}. Use /resume {thread_id} to open it."), None,
                            );
                        }
                        return Ok(control);
                    }
                    self.chat_widget.add_error_message(format!("Merge saved as {thread_id}, but could not open it. Use /resume {thread_id}."));
                }
            }
            Ok(None) => self
                .chat_widget
                .add_info_message("Conversation merge cancelled.".into(), None),
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Conversation merge failed: {error:#}")),
        }
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
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
        if app_server.uses_remote_workspace() {
            self.chat_widget
                .add_error_message("/tree currently needs local saved chats.".into());
            return Ok(AppRunControl::Continue);
        }
        let mut bridge = match MergeBridge::start(&self.config) {
            Ok(bridge) => bridge,
            Err(error) => {
                self.chat_widget.add_error_message(format!("{error:#}"));
                return Ok(AppRunControl::Continue);
            }
        };
        self.chat_widget
            .set_context_operation_status("Loading conversation tree", Some("Esc cancels.".into()));
        let outcome = self.run_context_operation(tui, app_server, &mut bridge, json!({
            "type": "start", "operation": "tree", "primaryThreadId": current_thread_id.to_string(), "args": [],
        }), "Loading conversation tree").await;
        self.chat_widget
            .set_context_operation_status("Loading conversation tree", None);
        bridge
            .finish(app_server.request_handle(), matches!(&outcome, Ok(Some(_))))
            .await;
        match outcome {
            Ok(Some(result)) => {
                let result: ContextTreeResult = serde_json::from_value(result)?;
                if !result.warnings.is_empty() {
                    self.chat_widget
                        .add_info_message(result.warnings.join("\n"), None);
                }
                if let Some(target) = target {
                    match result.rows.iter().find(|row| row.id == target) {
                        Some(row) if row.disabled_reason.is_none() => {
                            if let Ok(id) = ThreadId::from_string(&row.id) {
                                return self.switch_context_branch(tui, app_server, current_thread_id, id).await;
                            }
                        }
                        Some(row) => self.chat_widget.add_error_message(row.disabled_reason.clone().unwrap_or_else(|| "Invalid branch ID".into())),
                        None => self.chat_widget.add_error_message(format!("'{target}' is not in this conversation tree. Use /resume to open an unrelated chat.")),
                    }
                } else {
                    let current = result.rows.iter().position(|row| row.current);
                    let items = result
                        .rows
                        .into_iter()
                        .map(|row| {
                            let id = ThreadId::from_string(&row.id).ok();
                            let actions: Vec<crate::bottom_pane::SelectionAction> = id
                                .map(|target_thread_id| {
                                    Box::new(move |tx: &AppEventSender| {
                                        tx.send(AppEvent::SwitchContextBranch {
                                            current_thread_id,
                                            target_thread_id,
                                        })
                                    })
                                        as crate::bottom_pane::SelectionAction
                                })
                                .into_iter()
                                .collect();
                            SelectionItem {
                                name: row.name,
                                description: Some(row.description),
                                search_value: Some(row.search),
                                is_current: row.current,
                                disabled_reason: row.disabled_reason,
                                is_disabled: id.is_none(),
                                actions,
                                dismiss_on_select: true,
                                ..Default::default()
                            }
                        })
                        .collect();
                    self.chat_widget.show_context_tree_picker(items, current);
                }
            }
            Ok(None) => self
                .chat_widget
                .add_info_message("Conversation tree cancelled.".into(), None),
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Could not load conversation tree: {error:#}")),
        }
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
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

    async fn run_context_operation(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        bridge: &mut MergeBridge,
        start_message: Value,
        status_title: &str,
    ) -> Result<Option<Value>> {
        bridge.send(start_message).await?;
        let mut events = tui.event_stream();
        let mut cancelled = false;
        let mut cancel_deadline = None;
        tui.frame_requester().schedule_frame();
        loop {
            tokio::select! {
                line = bridge.output.next_line() => {
                    let line = line?.ok_or_else(|| eyre!("Conversation merge helper exited unexpectedly"))?;
                    match serde_json::from_str::<MergeMessage>(&line)? {
                        MergeMessage::Rpc { message } if message.get("method").is_some() => {
                            bridge.request(app_server.request_handle(), message);
                        }
                        MergeMessage::Rpc { message } => {
                            let id: RequestId = serde_json::from_value(message["id"].clone())?;
                            let error: JSONRPCErrorError = serde_json::from_value(message["error"].clone())?;
                            app_server.reject_server_request(id, error).await?;
                        }
                        MergeMessage::Progress { text } => {
                            if !cancelled {
                                self.chat_widget.set_context_operation_status(status_title, Some(text));
                            }
                        }
                        MergeMessage::Result { result } => return Ok((!cancelled).then_some(result)),
                        MergeMessage::Error { .. } if cancelled => return Ok(None),
                        MergeMessage::Error { message } => color_eyre::eyre::bail!(message),
                    }
                }
                rpc = bridge.requests.join_next(), if !bridge.requests.is_empty() => {
                    if let Some(rpc) = rpc {
                        let rpc = rpc?;
                        bridge.record_rpc(&rpc);
                        bridge.send(json!({"type": "rpc", "message": rpc.response})).await?;
                    }
                }
                event = app_server.next_event() => {
                    let event = event.ok_or_else(|| eyre!("App-server connection closed during merge"))?;
                    let helper_event;
                    match &event {
                        AppServerEvent::ServerNotification(notification) => {
                            helper_event = matches!(
                                server_notification_thread_target(notification.as_ref()),
                                ServerNotificationThreadTarget::Thread(id) if bridge.threads.contains_key(&id)
                            );
                            bridge.send(json!({"type": "rpc", "message": notification})).await?;
                        }
                        AppServerEvent::ServerRequest(request) => {
                            helper_event = server_request_thread_id(request.as_ref())
                                .is_some_and(|id| bridge.threads.contains_key(&id));
                            if helper_event {
                                bridge.send(json!({"type": "rpc", "message": request})).await?;
                            }
                        }
                        AppServerEvent::Disconnected { message } => color_eyre::eyre::bail!(message.clone()),
                        AppServerEvent::Lagged { .. } => color_eyre::eyre::bail!("App-server events were lost during merge; try again."),
                    }
                    if !helper_event {
                        self.handle_app_server_event(app_server, event).await;
                    }
                }
                event = events.next() => {
                    match event {
                        Some(TuiEvent::Key(key)) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                            && (key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))) => {
                            if !cancelled {
                                cancelled = true;
                                cancel_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(10));
                                bridge.send(json!({"type": "cancel"})).await?;
                                self.chat_widget.set_context_operation_status(status_title, Some("Cancelling…".into()));
                            }
                        }
                        Some(event @ (TuiEvent::Draw | TuiEvent::Resize(_) | TuiEvent::Resume | TuiEvent::FocusGained)) => {
                            let size = tui.screen_size_for_event(&event)?;
                            self.handle_draw_pre_render(tui, size)?;
                            self.chat_widget.pre_draw_tick();
                            self.render_chat_widget_frame(tui, size)?;
                        }
                        None => return Ok(None),
                        _ => {}
                    }
                }
                _ = async {
                    if let Some(deadline) = cancel_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => return Ok(None),
            }
        }
    }
}

async fn merge_picker_items(
    handle: AppServerRequestHandle,
    primary_thread_id: ThreadId,
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
                description: Some(format!("{id} · {}", thread["cwd"].as_str().unwrap_or(""))),
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
