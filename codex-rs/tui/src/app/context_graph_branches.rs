//! In-session branches from the context graph, when the `context_graph` feature is on.
//!
//! A fork of a recorded chat joins its context graph as a new branch. `/tree` lists the
//! branches of the current chat's graph and switches to one; `/merge` folds a branch's work into
//! the current chat, or with `--reference` lets recall choose from the branch's history. Both go
//! through the app server's `contextGraph/*` requests, so they work with remote servers too.

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ContextGraphBranch;
use codex_app_server_protocol::ContextGraphBranchRenameParams;
use codex_app_server_protocol::ContextGraphBranchStatus;
use codex_app_server_protocol::ContextGraphMergeMode;
use codex_app_server_protocol::ContextGraphMode;
use codex_app_server_protocol::ContextGraphReadResponse;
use codex_app_server_protocol::ContextGraphViewsReadResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::Turn;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

use super::*;
use crate::app_server_session::ForkGoalContinuation;
use crate::bottom_pane::MultiSelectItem;

/// One line about a branch: its turns, latest request and merges.
fn describe(graph: &ContextGraphReadResponse, branch: &ContextGraphBranch) -> String {
    let mut parts = vec![match branch.turns {
        1 => "1 turn".to_string(),
        turns => format!("{turns} turns"),
    }];
    if let Some(summary) = &branch.summary {
        parts.push(summary.clone());
    }
    if let Some(request) = &branch.last_request {
        parts.push(format!("last: {request}"));
    }
    let mut merged_into: Vec<&str> = Vec::new();
    let mut referenced_by: Vec<&str> = Vec::new();
    for merge in graph
        .merges
        .iter()
        .filter(|merge| merge.source_branch_id == branch.branch_id)
    {
        let target = graph
            .branches
            .iter()
            .find(|candidate| candidate.branch_id == merge.target_branch_id)
            .map_or(merge.target_branch_id.as_str(), |target| {
                target.name.as_str()
            });
        let targets = if merge.mode == ContextGraphMergeMode::Reference {
            &mut referenced_by
        } else {
            &mut merged_into
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    if !merged_into.is_empty() {
        parts.push(format!("merged into {}", merged_into.join(", ")));
    }
    if !referenced_by.is_empty() {
        parts.push(format!("referenced by {}", referenced_by.join(", ")));
    }
    if branch.status == ContextGraphBranchStatus::RolledBack {
        parts.push("rolled back".to_string());
    }
    parts.join(" · ")
}

/// The tree lines in front of each branch name: `├ ` or `└ ` before a fork, and `│ ` where an
/// ancestor has later siblings.
fn tree_labels(branches: &[ContextGraphBranch]) -> Vec<String> {
    let depths: Vec<u32> = branches.iter().map(|branch| branch.depth).collect();
    // Whether a later row at `depth` follows row `index` before the tree climbs above `depth`.
    let has_later_sibling = |index: usize, depth: u32| {
        depths[index + 1..]
            .iter()
            .take_while(|later| **later >= depth)
            .any(|later| *later == depth)
    };
    branches
        .iter()
        .enumerate()
        .map(|(index, branch)| {
            if branch.depth == 0 {
                return branch.name.clone();
            }
            let mut label = String::new();
            for level in 1..branch.depth {
                label.push_str(if has_later_sibling(index, level) {
                    "│ "
                } else {
                    "  "
                });
            }
            label.push_str(if has_later_sibling(index, branch.depth) {
                "├ "
            } else {
                "└ "
            });
            label.push_str(&branch.name);
            label
        })
        .collect()
}

/// The rows of the `/tree` picker; selecting one switches `current_thread_id` to its chat.
fn tree_items(graph: &ContextGraphReadResponse, current_thread_id: ThreadId) -> Vec<SelectionItem> {
    let labels = tree_labels(&graph.branches);
    graph
        .branches
        .iter()
        .zip(labels)
        .map(|(branch, label)| {
            let target_thread_id = openable_thread(branch);
            let actions: Vec<crate::bottom_pane::SelectionAction> = target_thread_id
                .map(|target_thread_id| {
                    Box::new(move |tx: &AppEventSender| {
                        tx.send(AppEvent::SwitchContextBranch {
                            current_thread_id,
                            target_thread_id,
                        })
                    }) as crate::bottom_pane::SelectionAction
                })
                .into_iter()
                .collect();
            SelectionItem {
                name: label,
                description: Some(describe(graph, branch)),
                search_value: Some(format!(
                    "{} {} {}",
                    branch.name,
                    branch.branch_id,
                    branch.thread_id.as_deref().unwrap_or_default()
                )),
                is_current: branch.current,
                is_disabled: target_thread_id.is_none(),
                disabled_reason: target_thread_id
                    .is_none()
                    .then(|| "This history has no chat to open.".to_string()),
                actions,
                dismiss_on_select: true,
                ..Default::default()
            }
        })
        .collect()
}

/// What `/context` shows: the mode, the branch, and the latest recalls or why they could not
/// be read.
fn context_status(
    graph: &ContextGraphReadResponse,
    views: Result<&ContextGraphViewsReadResponse, &String>,
) -> Vec<String> {
    let mut lines = vec![match graph.mode {
        Some(mode) => format!("Context mode: {}.", mode_name(mode)),
        None => "Context mode: unknown; this chat is not loaded.".to_string(),
    }];
    if let Some(current) = graph.branches.iter().find(|branch| branch.current) {
        lines.push(format!(
            "Branch {} of {} in this conversation.",
            current.name,
            graph.branches.len()
        ));
    }
    match views {
        Ok(views) if views.views.is_empty() => {
            lines.push("No recalled context yet.".to_string());
        }
        Ok(views) => {
            lines.push("Recent recalls:".to_string());
            for view in &views.views {
                let question: String = view.question.chars().take(60).collect();
                lines.push(format!(
                    "  {} blocks, about {} of {} tokens, {:.1}s: {question}",
                    view.blocks,
                    view.tokens,
                    view.budget_tokens,
                    view.latency_ms as f64 / 1000.0
                ));
            }
        }
        Err(error) => lines.push(format!("Could not read recalled views: {error}")),
    }
    lines
}

/// The thread to open for a branch, unless its history has no chat of its own.
fn openable_thread(branch: &ContextGraphBranch) -> Option<ThreadId> {
    if branch.status == ContextGraphBranchStatus::RolledBack {
        return None;
    }
    branch
        .thread_id
        .as_deref()
        .and_then(|id| ThreadId::from_string(id).ok())
}

fn find<'a>(graph: &'a ContextGraphReadResponse, name: &str) -> Option<&'a ContextGraphBranch> {
    let name = name.trim().trim_start_matches('@');
    graph.branches.iter().find(|branch| {
        branch.branch_id == name || branch.name == name || branch.thread_id.as_deref() == Some(name)
    })
}

/// Splits `/merge` arguments into branch names and the merge mode.
fn merge_args(args: Vec<String>) -> (Vec<String>, ContextGraphMergeMode) {
    let mut mode = ContextGraphMergeMode::Fold;
    let mut names = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--reference" | "-r" => mode = ContextGraphMergeMode::Reference,
            "--synthesize" | "-s" => mode = ContextGraphMergeMode::Synthesize,
            "--inline" => mode = ContextGraphMergeMode::Inline,
            "--fold" => mode = ContextGraphMergeMode::Fold,
            _ => names.push(arg),
        }
    }
    (names, mode)
}

fn mode_name(mode: ContextGraphMode) -> &'static str {
    match mode {
        ContextGraphMode::Full => "full",
        ContextGraphMode::Auto => "auto",
        ContextGraphMode::Recall => "recall",
    }
}

impl App {
    /// Whether `/tree` and `/merge` work on the context graph instead of saved chats.
    pub(super) fn context_graph_branches_enabled(&self) -> bool {
        self.config.features.enabled(Feature::ContextGraph)
    }

    async fn read_context_graph(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> Option<ContextGraphReadResponse> {
        match app_server.context_graph_read(thread_id).await {
            Ok(graph) if graph.graph_id.is_some() => Some(graph),
            Ok(_) => {
                self.chat_widget.add_info_message(
                    "This chat has no recorded branches yet. Use /fork to start one.".into(),
                    None,
                );
                None
            }
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not load the branch tree: {error:#}"));
                None
            }
        }
    }

    pub(super) async fn open_graph_tree(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        current_thread_id: ThreadId,
        target: Option<String>,
    ) -> Result<AppRunControl> {
        let Some(graph) = self.read_context_graph(app_server, current_thread_id).await else {
            return Ok(AppRunControl::Continue);
        };
        if let Some(target) = target {
            let branch = find(&graph, &target);
            return match branch.map(|branch| (branch, openable_thread(branch))) {
                Some((_, Some(target_thread_id))) => {
                    self.switch_context_branch(tui, app_server, current_thread_id, target_thread_id)
                        .await
                }
                Some((branch, None)) => {
                    self.chat_widget
                        .add_error_message(format!("Branch {} has no chat to open.", branch.name));
                    Ok(AppRunControl::Continue)
                }
                None => {
                    self.chat_widget.add_error_message(format!(
                        "'{target}' is not a branch of this chat. Run /tree to list them."
                    ));
                    Ok(AppRunControl::Continue)
                }
            };
        }

        let current = graph.branches.iter().position(|branch| branch.current);
        let items = tree_items(&graph, current_thread_id);
        self.chat_widget.show_context_tree_picker(items, current);
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
    }

    /// Merges the branches `args` names into the current chat, folding them unless `args` has
    /// `--reference`; without names, opens a picker.
    pub(super) async fn merge_graph_branches(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        primary_thread_id: ThreadId,
        args: Vec<String>,
    ) -> Result<AppRunControl> {
        let (names, mode) = merge_args(args);
        if names.is_empty() {
            let Some(graph) = self.read_context_graph(app_server, primary_thread_id).await else {
                return Ok(AppRunControl::Continue);
            };
            let items: Vec<MultiSelectItem> = graph
                .branches
                .iter()
                .filter(|branch| {
                    !branch.current && branch.status != ContextGraphBranchStatus::RolledBack
                })
                .map(|branch| MultiSelectItem {
                    id: branch.branch_id.clone(),
                    name: branch.name.clone(),
                    description: Some(describe(&graph, branch)),
                    orderable: false,
                    ..Default::default()
                })
                .collect();
            if items.is_empty() {
                self.chat_widget
                    .add_info_message("This chat has no other branches.".into(), None);
            } else {
                let carried = match mode {
                    ContextGraphMergeMode::Reference => vec!["--reference".to_string()],
                    ContextGraphMergeMode::Synthesize => vec!["--synthesize".to_string()],
                    ContextGraphMergeMode::Inline => vec!["--inline".to_string()],
                    ContextGraphMergeMode::Fold => Vec::new(),
                };
                self.chat_widget
                    .show_conversation_merge_picker(primary_thread_id, items, carried);
            }
            return Ok(AppRunControl::Continue);
        }

        for name in names {
            match app_server
                .context_graph_merge(primary_thread_id, name.clone(), mode)
                .await
            {
                Ok(merged) => {
                    let turns = match merged.turns {
                        1 => "1 turn".to_string(),
                        turns => format!("{turns} turns"),
                    };
                    let (verb, hint) = match mode {
                        ContextGraphMergeMode::Fold => (
                            "Merged",
                            "The next turn sees a summary of what the branch did.",
                        ),
                        ContextGraphMergeMode::Synthesize => (
                            "Merged",
                            "The next turn sees a merge note a model wrote from the branch.",
                        ),
                        ContextGraphMergeMode::Inline => {
                            ("Merged", "The next turn sees the branch's history itself.")
                        }
                        ContextGraphMergeMode::Reference => (
                            "Referenced",
                            "Recall can now choose from the branch's history.",
                        ),
                    };
                    self.chat_widget.add_info_message(
                        format!("{verb} branch {} ({turns}).", merged.branch_name),
                        Some(hint.into()),
                    );
                }
                Err(error) => self
                    .chat_widget
                    .add_error_message(format!("Could not merge {name}: {error:#}")),
            }
        }
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
    }

    /// Names the branch of a new fork once its recorder has stored it.
    pub(super) fn name_forked_branch(
        &self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        name: String,
    ) {
        if !self.context_graph_branches_enabled() {
            return;
        }
        let handle = app_server.request_handle();
        tokio::spawn(async move {
            let request = ClientRequest::ContextGraphBranchRename {
                request_id: RequestId::String(format!("context-graph-fork-name-{thread_id}")),
                params: ContextGraphBranchRenameParams {
                    thread_id: thread_id.to_string(),
                    name,
                },
            };
            match handle.request(request).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!("could not name the forked branch: {error:?}"),
                Err(error) => tracing::warn!("could not name the forked branch: {error}"),
            }
        });
    }
}

impl App {
    /// Shows what folding the branch `branch` into `thread_id` would add.
    pub(super) async fn preview_context_branch(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        branch: String,
    ) {
        if !self.context_graph_branches_enabled() {
            self.chat_widget
                .add_error_message("Turn on the context_graph feature to use branches.".into());
            return;
        }
        match app_server
            .context_graph_branch_preview(thread_id, branch)
            .await
        {
            Ok(preview) => match preview.summary {
                Some(summary) => self.chat_widget.add_info_message(
                    summary,
                    Some(format!(
                        "/merge {} folds this in; /tree {} switches to it.",
                        preview.branch_name, preview.branch_name
                    )),
                ),
                None => self.chat_widget.add_info_message(
                    format!(
                        "Branch {} has no work this chat lacks.",
                        preview.branch_name
                    ),
                    None,
                ),
            },
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Could not preview the branch: {error:#}")),
        }
    }

    /// Reads the name of `thread_id`'s branch for the status line.
    pub(super) async fn refresh_context_branch_name(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        let name = if self.context_graph_branches_enabled() {
            match app_server.context_graph_read(thread_id).await {
                Ok(graph) => graph
                    .branches
                    .into_iter()
                    .find(|branch| branch.current)
                    .map(|branch| branch.name),
                Err(error) => {
                    tracing::debug!("could not read the context branch: {error:#}");
                    None
                }
            }
        } else {
            None
        };
        self.chat_widget.set_context_branch_name(thread_id, name);
    }

    /// Shows the current chat's branch, or renames it to `name`.
    pub(super) async fn context_branch(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        name: Option<String>,
    ) {
        if !self.context_graph_branches_enabled() {
            self.chat_widget
                .add_error_message("Turn on the context_graph feature to use branches.".into());
            return;
        }
        if let Some(name) = name {
            match app_server
                .context_graph_branch_rename(thread_id, name.clone())
                .await
            {
                Ok(()) => self
                    .chat_widget
                    .add_info_message(format!("This branch is now called {name}."), None),
                Err(error) => self
                    .chat_widget
                    .add_error_message(format!("Could not rename the branch: {error:#}")),
            }
            return;
        }
        let Some(graph) = self.read_context_graph(app_server, thread_id).await else {
            return;
        };
        let Some(current) = graph.branches.iter().find(|branch| branch.current) else {
            return;
        };
        let parent = current.parent_branch_id.as_ref().and_then(|parent| {
            graph
                .branches
                .iter()
                .find(|branch| &branch.branch_id == parent)
        });
        let mut lines = vec![format!(
            "Branch {}: {}",
            current.name,
            describe(&graph, current)
        )];
        if let Some(parent) = parent {
            lines.push(format!("Forked from {}.", parent.name));
        }
        let referenced: Vec<&str> = graph
            .merges
            .iter()
            .filter(|merge| {
                merge.target_branch_id == current.branch_id
                    && merge.mode == ContextGraphMergeMode::Reference
            })
            .filter_map(|merge| {
                graph
                    .branches
                    .iter()
                    .find(|branch| branch.branch_id == merge.source_branch_id)
                    .map(|branch| branch.name.as_str())
            })
            .collect();
        if !referenced.is_empty() {
            lines.push(format!("References {}.", referenced.join(", ")));
        }
        lines.push(format!(
            "{} branches in this conversation; /tree lists them.",
            graph.branches.len()
        ));
        self.chat_widget.add_info_message(lines.join("\n"), None);
    }

    /// Shows how the current chat manages its context, or switches it to `mode`.
    pub(super) async fn context_mode(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        mode: Option<String>,
    ) {
        if !self.context_graph_branches_enabled() {
            self.chat_widget
                .add_error_message("Turn on the context_graph feature to use recall.".into());
            return;
        }
        if let Some(mode) = mode {
            let mode = match mode.trim() {
                "full" => ContextGraphMode::Full,
                "auto" => ContextGraphMode::Auto,
                "recall" => ContextGraphMode::Recall,
                _ => {
                    self.chat_widget
                        .add_error_message("Usage: /context [full|auto|recall]".into());
                    return;
                }
            };
            match app_server.context_graph_mode_set(thread_id, mode).await {
                Ok(()) => self.chat_widget.add_info_message(
                    format!("Context mode is now {}.", mode_name(mode)),
                    Some("It applies from the next turn.".into()),
                ),
                Err(error) => self
                    .chat_widget
                    .add_error_message(format!("Could not switch the context mode: {error:#}")),
            }
            return;
        }
        let graph = match app_server.context_graph_read(thread_id).await {
            Ok(graph) => graph,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not read the context graph: {error:#}"));
                return;
            }
        };
        let views = app_server
            .context_graph_views_read(thread_id, 3)
            .await
            .map_err(|error| format!("{error:#}"));
        let lines = context_status(&graph, views.as_ref());
        self.chat_widget.add_info_message(
            lines.join("\n"),
            Some("/context full|auto|recall switches the mode.".into()),
        );
    }
}

impl App {
    /// Forks the current chat into a branch that works on `prompt` while this chat stays open, in
    /// a new worktree when `worktree` is set. The branch reports here when its turn ends.
    pub(super) async fn fork_background_branch(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        name: Option<String>,
        worktree: bool,
        prompt: String,
    ) -> Result<AppRunControl> {
        let Some(source_thread_id) = self.chat_widget.thread_id() else {
            self.chat_widget.add_error_message(
                "Session is still starting; try /fork again in a moment.".into(),
            );
            return Ok(AppRunControl::Continue);
        };
        if self.chat_widget.is_user_turn_pending_or_running() {
            self.chat_widget.add_error_message(
                "Wait for the current turn to finish before forking a background branch.".into(),
            );
            return Ok(AppRunControl::Continue);
        }
        self.refresh_in_memory_config_from_disk_best_effort("forking a background branch")
            .await;
        let mut checkout = None;
        let mut fork_config = if worktree {
            match self.create_branch_worktree().await {
                Ok((manager, created)) => {
                    let config = self.rebuild_config_for_cwd(created.cwd.clone()).await;
                    checkout = Some((manager, created));
                    match config {
                        Ok(config) if config.active_project.trust_level.is_some() => config,
                        Ok(_) | Err(_) => {
                            self.chat_widget.add_error_message(
                                "The new worktree is not trusted; run Codex there first.".into(),
                            );
                            return Ok(AppRunControl::Continue);
                        }
                    }
                }
                Err(error) => {
                    self.chat_widget
                        .add_error_message(format!("Could not create a worktree: {error:#}"));
                    return Ok(AppRunControl::Continue);
                }
            }
        } else {
            self.config.clone()
        };
        fork_config
            .model_provider_id
            .clone_from(&self.chat_widget.config_ref().model_provider_id);
        fork_config.model = Some(self.chat_widget.current_model().to_string());
        fork_config.model_reasoning_effort = self.chat_widget.current_reasoning_effort();
        let selected_profile = self.selected_server_profile(source_thread_id);
        let forked = match app_server
            .fork_thread_at(
                &self.local_settings,
                fork_config,
                source_thread_id,
                /*last_turn_id*/ None,
                /*before_turn_id*/ None,
                ForkGoalContinuation::DeferUntilNextTurn,
                selected_profile.as_ref(),
            )
            .await
        {
            Ok(forked) => forked,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not fork a background branch: {error:#}"));
                return Ok(AppRunControl::Continue);
            }
        };
        let thread_id = forked.session.thread_id;
        if let Some((manager, created)) = &checkout
            && let Err(error) = manager.bind_thread(&created.root, &thread_id.to_string())
        {
            tracing::warn!("could not bind the branch worktree: {error:#}");
        }
        let name = name.unwrap_or_else(|| {
            let id = thread_id.to_string();
            let short = id
                .char_indices()
                .rev()
                .nth(7)
                .map_or(id.as_str(), |(start, _)| &id[start..]);
            format!("bg-{short}")
        });
        if let Err(error) = app_server.thread_set_name(thread_id, name.clone()).await {
            tracing::warn!("could not name the background branch: {error:#}");
        }
        self.name_forked_branch(app_server, thread_id, name.clone());
        let turn = self
            .chat_widget
            .branch_user_turn(prompt, forked.session.cwd.to_path_buf());
        if let Err(error) = self.submit_thread_op(app_server, thread_id, turn).await {
            self.chat_widget
                .add_error_message(format!("Could not start the background branch: {error:#}"));
            return Ok(AppRunControl::Continue);
        }
        // Routing the branch through its own channel queues its approvals until it is opened.
        self.ensure_thread_channel(thread_id);
        self.background_branches.insert(thread_id, name.clone());
        let place = match &checkout {
            Some((_, created)) => format!(" in {}", created.cwd.display()),
            None => String::new(),
        };
        self.chat_widget.add_info_message(
            format!("Started background branch {name}{place}."),
            Some(format!(
                "You will be told when its turn ends. /tree switches to it; /merge {name} folds its work in."
            )),
        );
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
    }

    async fn create_branch_worktree(
        &self,
    ) -> anyhow::Result<(
        codex_worktree::WorktreeManager,
        codex_worktree::ManagedWorktree,
    )> {
        anyhow::ensure!(
            self.config.features.enabled(Feature::Worktrees),
            "enable worktrees in your Codex configuration to fork into one"
        );
        let host = crate::legacy_core::config::load_config_toml_with_layer_stack(
            &self.config.codex_home,
            /*cwd*/ None,
            Vec::new(),
            codex_config::ConfigLoadOptions::default(),
        )
        .await?;
        let settings = codex_worktree::WorktreeSettings::for_cli(
            &self.config.codex_home,
            host.config_toml.desktop.as_ref(),
        )?;
        let manager = codex_worktree::WorktreeManager::new(settings);
        let source_cwd = self.config.cwd.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let created = manager.create(&codex_worktree::CreateWorktree {
                source_cwd,
                base: None,
            })?;
            anyhow::Ok((manager, created))
        })
        .await?
    }

    /// Reports the end of a background branch's turn and, with `on_branch_complete = "fold"`,
    /// folds its work into the chat that started it. The notification is not consumed.
    pub(super) async fn handle_background_branch_notification(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) -> bool {
        let Some(name) = self.background_branches.get(&thread_id).cloned() else {
            return false;
        };
        if !matches!(notification, ServerNotification::TurnCompleted(_)) {
            return false;
        }
        let fold = self.config.context_graph.on_branch_complete
            == codex_config::types::BranchCompletion::Fold;
        let primary = self.chat_widget.thread_id();
        match (fold, primary) {
            (true, Some(primary)) => {
                let request = ClientRequest::ContextGraphMerge {
                    request_id: RequestId::String(format!("context-graph-fold-{thread_id}")),
                    params: codex_app_server_protocol::ContextGraphMergeParams {
                        thread_id: primary.to_string(),
                        branch: thread_id.to_string(),
                        mode: ContextGraphMergeMode::Fold,
                    },
                };
                match app_server.request_handle().request(request).await {
                    Ok(Ok(_)) => self.chat_widget.add_info_message(
                        format!("Background branch {name} finished; its work was folded in."),
                        None,
                    ),
                    Ok(Err(error)) => self.chat_widget.add_error_message(format!(
                        "Background branch {name} finished, but folding it failed: {}",
                        error.message
                    )),
                    Err(error) => self.chat_widget.add_error_message(format!(
                        "Background branch {name} finished, but folding it failed: {error}"
                    )),
                }
            }
            _ => self.chat_widget.add_info_message(
                format!("Background branch {name} finished its turn."),
                Some(format!(
                    "/tree switches to it; /merge {name} folds its work in."
                )),
            ),
        }
        // The notification still reaches the branch's own channel.
        false
    }
}

impl App {
    /// The configuration a fork starts from: the current one with this chat's model settings.
    fn branch_fork_config(&self) -> Config {
        let mut config = self.config.clone();
        config
            .model_provider_id
            .clone_from(&self.chat_widget.config_ref().model_provider_id);
        config.model = Some(self.chat_widget.current_model().to_string());
        config.model_reasoning_effort = self.chat_widget.current_reasoning_effort();
        config
    }

    /// Forks the current chat after its turn `turn`, counted from 1, and switches to the fork.
    pub(super) async fn fork_at_turn(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        turn: usize,
        name: Option<String>,
    ) -> Result<AppRunControl> {
        let Some(thread_id) = self.chat_widget.thread_id() else {
            self.chat_widget.add_error_message(
                "Session is still starting; try /fork again in a moment.".into(),
            );
            return Ok(AppRunControl::Continue);
        };
        if self.chat_widget.is_user_turn_pending_or_running() {
            self.chat_widget.add_error_message(
                "Wait for the current turn to finish before forking from an earlier turn.".into(),
            );
            return Ok(AppRunControl::Continue);
        }
        let thread = match app_server
            .thread_read(thread_id, /*include_turns*/ true)
            .await
        {
            Ok(thread) => thread,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not read this chat's turns: {error:#}"));
                return Ok(AppRunControl::Continue);
            }
        };
        let Some(last_turn) = turn
            .checked_sub(1)
            .and_then(|index| thread.turns.get(index))
        else {
            self.chat_widget.add_error_message(format!(
                "This chat has {} turns; /fork --at takes a number from 1 to {}.",
                thread.turns.len(),
                thread.turns.len()
            ));
            return Ok(AppRunControl::Continue);
        };
        let selected_profile = self.selected_server_profile(thread_id);
        let forked = match app_server
            .fork_thread_at(
                &self.local_settings,
                self.branch_fork_config(),
                thread_id,
                Some(last_turn.id.clone()),
                /*before_turn_id*/ None,
                ForkGoalContinuation::DeferUntilNextTurn,
                selected_profile.as_ref(),
            )
            .await
        {
            Ok(forked) => forked,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Could not fork at turn {turn}: {error:#}"));
                return Ok(AppRunControl::Continue);
            }
        };
        let fork_id = forked.session.thread_id;
        let name = name.unwrap_or_else(|| format!("turn-{turn}"));
        if let Err(error) = app_server.thread_set_name(fork_id, name.clone()).await {
            tracing::warn!("could not name the fork: {error:#}");
        }
        self.name_forked_branch(app_server, fork_id, name.clone());
        let control = self
            .switch_context_branch(tui, app_server, thread_id, fork_id)
            .await?;
        if self.chat_widget.thread_id() == Some(fork_id) {
            self.chat_widget.add_info_message(
                format!("Forked branch {name} after turn {turn}."),
                Some("The original chat is unchanged; /tree switches back.".into()),
            );
        }
        Ok(control)
    }

    /// Edits a past prompt in a new branch forked before its turn, so the original conversation
    /// stays as it was.
    pub(super) async fn edit_prompt_in_branch(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        before_turn_id: String,
        prompt: crate::chatwidget::UserMessage,
    ) -> Result<AppRunControl> {
        let selected_profile = self.selected_server_profile(thread_id);
        let forked = match app_server
            .fork_thread_at(
                &self.local_settings,
                self.branch_fork_config(),
                thread_id,
                /*last_turn_id*/ None,
                Some(before_turn_id),
                ForkGoalContinuation::DeferUntilNextTurn,
                selected_profile.as_ref(),
            )
            .await
        {
            Ok(forked) => forked,
            Err(error) => {
                self.restore_backtrack_prompt_after_revert_error(prompt, error);
                tui.frame_requester().schedule_frame();
                return Ok(AppRunControl::Continue);
            }
        };
        let fork_id = forked.session.thread_id;
        let id = fork_id.to_string();
        let short = id
            .char_indices()
            .rev()
            .nth(7)
            .map_or(id.as_str(), |(start, _)| &id[start..]);
        let name = format!("edit-{short}");
        if let Err(error) = app_server.thread_set_name(fork_id, name.clone()).await {
            tracing::warn!("could not name the edit branch: {error:#}");
        }
        self.name_forked_branch(app_server, fork_id, name.clone());
        let control = self
            .switch_context_branch(tui, app_server, thread_id, fork_id)
            .await?;
        if self.chat_widget.thread_id() == Some(fork_id) {
            self.chat_widget.restore_user_message_to_composer(prompt);
            self.chat_widget.add_info_message(
                format!("Editing in a new branch, {name}."),
                Some("The original conversation stays as it was; /tree switches back.".into()),
            );
        } else {
            self.chat_widget.restore_user_message_to_composer(prompt);
        }
        tui.frame_requester().schedule_frame();
        Ok(control)
    }

    /// Folds the questions and answers of the side conversation on screen into its parent chat,
    /// then returns to the parent.
    pub(super) async fn fold_side_conversation(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
    ) -> Result<AppRunControl> {
        let Some(side_id) = self.current_displayed_thread_id() else {
            return Ok(AppRunControl::Continue);
        };
        let Some(parent_id) = self
            .side_threads
            .get(&side_id)
            .map(|state| state.parent_thread_id)
        else {
            self.chat_widget.add_error_message(
                "/merge here folds a side conversation; this is not one.".into(),
            );
            return Ok(AppRunControl::Continue);
        };
        let turns = match self.thread_event_channels.get(&side_id) {
            Some(channel) => channel.store.lock().await.turns.clone(),
            None => Vec::new(),
        };
        let Some(summary) = side_conversation_summary(&turns) else {
            self.chat_widget.add_info_message(
                "This side conversation has nothing to fold yet.".into(),
                None,
            );
            return Ok(AppRunControl::Continue);
        };
        let note = ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText { text: summary }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        if let Err(error) = app_server.thread_inject_items(parent_id, vec![note]).await {
            self.chat_widget
                .add_error_message(format!("Could not fold the side conversation: {error:#}"));
            return Ok(AppRunControl::Continue);
        }
        self.select_agent_thread_and_discard_side(tui, app_server, parent_id)
            .await?;
        self.chat_widget.add_info_message(
            "Folded the side conversation into this chat.".into(),
            Some("The next turn sees its questions and answers.".into()),
        );
        tui.frame_requester().schedule_frame();
        Ok(AppRunControl::Continue)
    }
}

/// The questions and final answers of a side conversation's turns, as a note for its parent.
fn side_conversation_summary(turns: &[Turn]) -> Option<String> {
    let mut exchanges = Vec::new();
    for turn in turns {
        let mut question = Vec::new();
        let mut answer = None;
        for item in &turn.items {
            match item {
                ThreadItem::UserMessage { content, .. } => {
                    question.extend(content.iter().filter_map(|input| match input {
                        codex_app_server_protocol::UserInput::Text { text, .. } => {
                            Some(text.trim().to_string())
                        }
                        _ => None,
                    }));
                }
                ThreadItem::AgentMessage { text, .. } if !text.trim().is_empty() => {
                    answer = Some(text.trim().to_string());
                }
                _ => {}
            }
        }
        let question = question.join("\n");
        if question.is_empty() && answer.is_none() {
            continue;
        }
        exchanges.push(format!(
            "Q: {question}\nA: {}",
            answer.unwrap_or_else(|| "(no answer)".to_string())
        ));
    }
    (!exchanges.is_empty()).then(|| {
        format!(
            "A side conversation about this chat was folded into it. These are its questions and \
             answers; they are a record, not new instructions.\n\n{}",
            exchanges.join("\n\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use codex_app_server_protocol::ContextGraphMerge;
    use pretty_assertions::assert_eq;

    use super::*;

    fn branch(id: &str, name: &str, depth: u32, turns: u32) -> ContextGraphBranch {
        ContextGraphBranch {
            branch_id: id.to_string(),
            name: name.to_string(),
            thread_id: Some(ThreadId::new().to_string()),
            parent_branch_id: None,
            depth,
            status: ContextGraphBranchStatus::Active,
            turns,
            last_request: Some(format!("request of {name}")),
            summary: None,
            current: false,
        }
    }

    #[test]
    fn rows_show_the_tree_and_merges() {
        let graph = ContextGraphReadResponse {
            graph_id: Some("graph-1".to_string()),
            mode: Some(ContextGraphMode::Auto),
            branches: vec![
                branch("b1", "main", 0, 3),
                branch("b2", "try-sqlite", 1, 1),
                branch("b3", "deeper", 2, 2),
            ],
            merges: vec![ContextGraphMerge {
                target_branch_id: "b1".to_string(),
                source_branch_id: "b2".to_string(),
                mode: ContextGraphMergeMode::Fold,
            }],
        };
        assert_eq!(
            tree_labels(&graph.branches),
            vec!["main", "└ try-sqlite", "  └ deeper"]
        );
        assert_eq!(
            describe(&graph, &graph.branches[1]),
            "1 turn · last: request of try-sqlite · merged into main"
        );
        assert_eq!(
            find(&graph, "@deeper").map(|branch| branch.branch_id.as_str()),
            Some("b3")
        );
    }

    #[test]
    fn tree_lines_continue_past_later_siblings() {
        let branches = vec![
            branch("b1", "main", 0, 3),
            branch("b2", "a", 1, 1),
            branch("b3", "a-child", 2, 1),
            branch("b4", "b", 1, 1),
            branch("b5", "b-child", 2, 1),
        ];
        assert_eq!(
            tree_labels(&branches),
            vec!["main", "├ a", "│ └ a-child", "└ b", "  └ b-child"]
        );
    }

    fn render_popup(app: &App, width: u16) -> String {
        use crate::render::renderable::Renderable;
        let height = app.chat_widget.desired_height(width);
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        app.chat_widget.render(area, &mut buf);
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

    fn sample_graph() -> ContextGraphReadResponse {
        let mut main = branch("b1", "main", 0, 4);
        main.current = true;
        main.summary = Some("HTTP client retries".to_string());
        let mut archived = branch("b4", "old-try", 1, 1);
        archived.thread_id = None;
        archived.status = ContextGraphBranchStatus::RolledBack;
        ContextGraphReadResponse {
            graph_id: Some("graph-1".to_string()),
            mode: Some(ContextGraphMode::Auto),
            branches: vec![
                main,
                branch("b2", "try-sqlite", 1, 2),
                branch("b3", "deeper", 2, 1),
                archived,
            ],
            merges: vec![ContextGraphMerge {
                target_branch_id: "b1".to_string(),
                source_branch_id: "b2".to_string(),
                mode: ContextGraphMergeMode::Reference,
            }],
        }
    }

    #[tokio::test]
    async fn tree_picker_shows_the_branches_as_a_tree() {
        let mut app = crate::app::test_support::make_test_app().await;
        let graph = sample_graph();
        let items = tree_items(&graph, ThreadId::new());
        app.chat_widget.show_context_tree_picker(items, Some(0));
        insta::with_settings!({snapshot_path => "../snapshots"}, {
            insta::assert_snapshot!("context_tree_picker", render_popup(&app, 90));
        });
    }

    #[test]
    fn context_status_lists_the_mode_branch_and_recalls() {
        let views = ContextGraphViewsReadResponse {
            views: vec![codex_app_server_protocol::ContextGraphView {
                question: "why did the retry test fail".to_string(),
                selector: "gpt-6-luna".to_string(),
                blocks: 7,
                tokens: 5_200,
                budget_tokens: 40_000,
                latency_ms: 2_350,
            }],
        };
        let none = ContextGraphViewsReadResponse { views: Vec::new() };
        let error = "connection closed".to_string();
        let text = [
            context_status(&sample_graph(), Ok(&views)).join("\n"),
            context_status(&sample_graph(), Ok(&none)).join("\n"),
            context_status(&sample_graph(), Err(&error)).join("\n"),
        ]
        .join("\n---\n");
        insta::with_settings!({snapshot_path => "../snapshots"}, {
            insta::assert_snapshot!("context_status", text);
        });
    }

    #[test]
    fn merge_arguments_select_the_mode() {
        assert_eq!(
            merge_args(vec!["--reference".to_string(), "try-sqlite".to_string()]),
            (
                vec!["try-sqlite".to_string()],
                ContextGraphMergeMode::Reference
            )
        );
        assert_eq!(
            merge_args(vec!["try-sqlite".to_string()]),
            (vec!["try-sqlite".to_string()], ContextGraphMergeMode::Fold)
        );
    }
}
