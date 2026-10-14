//! Requests about a conversation's context graph: its branch tree, merges and renames, and the
//! views recall built.

use codex_app_server_protocol::ContextGraphBlock;
use codex_app_server_protocol::ContextGraphBlockReadParams;
use codex_app_server_protocol::ContextGraphBlockReadResponse;
use codex_app_server_protocol::ContextGraphBranch;
use codex_app_server_protocol::ContextGraphBranchPreviewParams;
use codex_app_server_protocol::ContextGraphBranchPreviewResponse;
use codex_app_server_protocol::ContextGraphBranchReadParams;
use codex_app_server_protocol::ContextGraphBranchReadResponse;
use codex_app_server_protocol::ContextGraphBranchStatus;
use codex_app_server_protocol::ContextGraphBranchUpdateParams;
use codex_app_server_protocol::ContextGraphBranchUpdateResponse;
use codex_app_server_protocol::ContextGraphCodeMerge;
use codex_app_server_protocol::ContextGraphMerge;
use codex_app_server_protocol::ContextGraphMergeMode;
use codex_app_server_protocol::ContextGraphMergeParams;
use codex_app_server_protocol::ContextGraphMergeResponse;
use codex_app_server_protocol::ContextGraphMergedBranch;
use codex_app_server_protocol::ContextGraphMode;
use codex_app_server_protocol::ContextGraphModeSetParams;
use codex_app_server_protocol::ContextGraphModeSetResponse;
use codex_app_server_protocol::ContextGraphReadParams;
use codex_app_server_protocol::ContextGraphReadResponse;
use codex_app_server_protocol::ContextGraphRecallFeedback;
use codex_app_server_protocol::ContextGraphRecallParams;
use codex_app_server_protocol::ContextGraphRecallResponse;
use codex_app_server_protocol::ContextGraphSearchHit;
use codex_app_server_protocol::ContextGraphSearchParams;
use codex_app_server_protocol::ContextGraphSearchResponse;
use codex_app_server_protocol::ContextGraphTurn;
use codex_app_server_protocol::ContextGraphUpdatedNotification;
use codex_app_server_protocol::ContextGraphView;
use codex_app_server_protocol::ContextGraphViewEntry;
use codex_app_server_protocol::ContextGraphViewsReadParams;
use codex_app_server_protocol::ContextGraphViewsReadResponse;
use codex_config::types::ContextMode;
use codex_context_graph::BranchStatus;
use codex_context_graph::branches::BlockRow;
use codex_context_graph::branches::branch_boundary_note;
use codex_context_graph::render::kind_name;
use codex_context_graph_store::BranchTree;
use codex_context_graph_store::CodeMerge;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::MergeError;
use codex_context_graph_store::MergeMode;

use super::*;

const DEFAULT_VIEW_LIMIT: u32 = 20;
const DEFAULT_SEARCH_LIMIT: u32 = 20;

fn status(status: BranchStatus) -> ContextGraphBranchStatus {
    match status {
        BranchStatus::Active => ContextGraphBranchStatus::Active,
        BranchStatus::RolledBack => ContextGraphBranchStatus::RolledBack,
        BranchStatus::Merged => ContextGraphBranchStatus::Merged,
        BranchStatus::Archived => ContextGraphBranchStatus::Archived,
    }
}

fn merge_mode(mode: MergeMode) -> ContextGraphMergeMode {
    match mode {
        MergeMode::Fold => ContextGraphMergeMode::Fold,
        MergeMode::Reference => ContextGraphMergeMode::Reference,
        MergeMode::Synthesize => ContextGraphMergeMode::Synthesize,
        MergeMode::Inline => ContextGraphMergeMode::Inline,
    }
}

fn context_mode(mode: ContextMode) -> ContextGraphMode {
    match mode {
        ContextMode::Full => ContextGraphMode::Full,
        ContextMode::Auto => ContextGraphMode::Auto,
        ContextMode::Recall => ContextGraphMode::Recall,
    }
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn block(row: &BlockRow) -> ContextGraphBlock {
    ContextGraphBlock {
        block_id: row.id.to_string(),
        kind: kind_name(row.kind).to_string(),
        title: row.title.clone(),
        tokens: row.tokens,
    }
}

fn code_merge(code: CodeMerge) -> ContextGraphCodeMerge {
    ContextGraphCodeMerge {
        git_branch: code.git_branch,
        snapshot_commit: code.snapshot_commit,
        merge_commit: code.merge_commit,
        conflicts: code.conflicts,
        files: code.files,
    }
}

fn read_error(err: anyhow::Error) -> JSONRPCErrorError {
    internal_error(format!("failed to read the context graph: {err:#}"))
}

/// A `contextGraph/*` request.
pub(crate) enum ContextGraphRequest {
    Read(ContextGraphReadParams),
    Merge(ContextGraphMergeParams),
    BranchUpdate(ContextGraphBranchUpdateParams),
    BranchRead(ContextGraphBranchReadParams),
    BlockRead(ContextGraphBlockReadParams),
    Search(ContextGraphSearchParams),
    Recall(ContextGraphRecallParams),
    BranchPreview(ContextGraphBranchPreviewParams),
    ModeSet(ContextGraphModeSetParams),
    ViewsRead(ContextGraphViewsReadParams),
}

type ContextGraphResult = Result<Option<ClientResponsePayload>, JSONRPCErrorError>;

impl TurnRequestProcessor {
    /// Handles a `contextGraph/*` request. A plain function, so the handler's future is built
    /// and boxed in this frame rather than in the request dispatcher's.
    pub(crate) fn context_graph_request<'a>(
        &'a self,
        request_id: &'a ConnectionRequestId,
        request: ContextGraphRequest,
    ) -> futures::future::BoxFuture<'a, ContextGraphResult> {
        match request {
            ContextGraphRequest::Read(params) => Box::pin(self.context_graph_read(params)),
            ContextGraphRequest::Merge(params) => {
                Box::pin(self.context_graph_merge(request_id, params))
            }
            ContextGraphRequest::BranchUpdate(params) => {
                Box::pin(self.context_graph_branch_update(params))
            }
            ContextGraphRequest::BranchRead(params) => {
                Box::pin(self.context_graph_branch_read(params))
            }
            ContextGraphRequest::BlockRead(params) => {
                Box::pin(self.context_graph_block_read(params))
            }
            ContextGraphRequest::Search(params) => Box::pin(self.context_graph_search(params)),
            ContextGraphRequest::Recall(params) => Box::pin(self.context_graph_recall(params)),
            ContextGraphRequest::BranchPreview(params) => {
                Box::pin(self.context_graph_branch_preview(params))
            }
            ContextGraphRequest::ModeSet(params) => Box::pin(self.context_graph_mode_set(params)),
            ContextGraphRequest::ViewsRead(params) => {
                Box::pin(self.context_graph_views_read(params))
            }
        }
    }

    async fn context_graph_store(&self) -> Result<LocalContextGraphStore, JSONRPCErrorError> {
        LocalContextGraphStore::open(&self.config.sqlite)
            .await
            .map_err(|err| internal_error(format!("context graph unavailable: {err:#}")))
    }

    async fn context_graph_read(
        &self,
        params: ContextGraphReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.context_graph_store().await?;
        let tree = store
            .branch_tree(&params.thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to read the context graph: {err:#}")))?;
        // Only a loaded thread has a mode; a saved one gets its mode when it is loaded.
        let mode = match ThreadId::from_string(&params.thread_id) {
            Ok(thread_id) => self
                .thread_manager
                .get_thread(thread_id)
                .await
                .ok()
                .and_then(|thread| thread.context_mode())
                .map(context_mode),
            Err(_) => None,
        };
        let Some(tree) = tree else {
            return Ok(Some(
                ContextGraphReadResponse {
                    graph_id: None,
                    branches: Vec::new(),
                    merges: Vec::new(),
                    mode,
                }
                .into(),
            ));
        };
        let branches = tree
            .rows()
            .into_iter()
            .map(|row| ContextGraphBranch {
                purpose: tree.settings_of(&row).purpose,
                parent_branch_id: tree
                    .graph
                    .branch(&row.branch)
                    .and_then(|branch| branch.parent.as_ref())
                    .map(ToString::to_string),
                branch_id: row.branch.to_string(),
                name: row.name,
                thread_id: row.thread_id,
                depth: to_u32(row.depth),
                status: status(row.status),
                turns: to_u32(row.turns),
                last_request: row.last_request,
                summary: tree
                    .branch_cards
                    .get(&row.branch)
                    .map(|card| card.title.clone()),
                current: row.current,
            })
            .collect();
        let merges = tree
            .merges
            .iter()
            .map(|merge| ContextGraphMerge {
                target_branch_id: merge.target.to_string(),
                source_branch_id: merge.source.to_string(),
                mode: merge_mode(merge.mode),
            })
            .collect();
        Ok(Some(
            ContextGraphReadResponse {
                graph_id: Some(tree.graph_id),
                branches,
                merges,
                mode,
            }
            .into(),
        ))
    }

    async fn context_graph_merge(
        &self,
        request_id: &ConnectionRequestId,
        params: ContextGraphMergeParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let mode = match params.mode {
            ContextGraphMergeMode::Fold => MergeMode::Fold,
            ContextGraphMergeMode::Reference => MergeMode::Reference,
            ContextGraphMergeMode::Synthesize => MergeMode::Synthesize,
            ContextGraphMergeMode::Inline => MergeMode::Inline,
        };
        let goal = params
            .goal
            .map(|goal| goal.trim().to_string())
            .filter(|goal| !goal.is_empty());
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        self.ensure_direct_input_allowed(request_id, thread.as_ref())
            .await?;
        let merged = thread
            .merge_context_branches(&params.branches, mode, goal, params.code)
            .await
            .map_err(|err| invalid_request(format!("{err:#}")))?;
        self.notify_graph_updated(params.thread_id, merged.graph_id)
            .await;
        Ok(Some(
            ContextGraphMergeResponse {
                merged: merged
                    .merged
                    .into_iter()
                    .map(|merged| ContextGraphMergedBranch {
                        branch_id: merged.branch_id,
                        branch_name: merged.branch_name,
                        turns: to_u32(merged.turns),
                        code: merged.code.map(code_merge),
                    })
                    .collect(),
            }
            .into(),
        ))
    }

    async fn notify_graph_updated(&self, thread_id: String, graph_id: String) {
        self.outgoing
            .send_server_notification(ServerNotification::ContextGraphUpdated(
                ContextGraphUpdatedNotification {
                    thread_id,
                    graph_id,
                },
            ))
            .await;
    }

    async fn branch_tree_of(&self, thread_id: &str) -> Result<BranchTree, JSONRPCErrorError> {
        self.context_graph_store()
            .await?
            .branch_tree(thread_id)
            .await
            .map_err(read_error)?
            .ok_or_else(|| invalid_request("this thread is not recorded yet".to_string()))
    }

    async fn context_graph_mode_set(
        &self,
        params: ContextGraphModeSetParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let mode = match params.mode {
            ContextGraphMode::Full => ContextMode::Full,
            ContextGraphMode::Auto => ContextMode::Auto,
            ContextGraphMode::Recall => ContextMode::Recall,
        };
        thread
            .set_context_mode(mode)
            .map_err(|err| invalid_request(format!("{err:#}")))?;
        Ok(Some(ContextGraphModeSetResponse {}.into()))
    }

    async fn context_graph_branch_update(
        &self,
        params: ContextGraphBranchUpdateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.context_graph_store().await?;
        // The branch of another thread is named by the conversation; the thread's own branch
        // can be changed before it first records.
        let (thread_id, branch_name, parent_name) = match params.branch.as_deref() {
            Some(branch) => {
                let tree = self.branch_tree_of(&params.thread_id).await?;
                let row = tree
                    .find(branch)
                    .ok_or_else(|| invalid_request(format!("no branch named {branch}")))?;
                let thread_id = row.thread_id.clone().ok_or_else(|| {
                    invalid_request(format!("branch {} has no chat to change", row.name))
                })?;
                let parent = tree
                    .graph
                    .branch(&row.branch)
                    .and_then(|branch| branch.parent.as_ref())
                    .map(|parent| tree.branch_name(parent));
                (thread_id, Some(row.name), parent)
            }
            None => {
                let tree = store
                    .branch_tree(&params.thread_id)
                    .await
                    .map_err(read_error)?;
                let row = tree
                    .as_ref()
                    .and_then(|tree| tree.rows().into_iter().find(|row| row.current));
                let parent = tree.as_ref().and_then(|tree| {
                    tree.graph
                        .branch(&tree.current)
                        .and_then(|branch| branch.parent.as_ref())
                        .map(|parent| tree.branch_name(parent))
                });
                (params.thread_id.clone(), row.map(|row| row.name), parent)
            }
        };
        let write_error =
            |err: anyhow::Error| internal_error(format!("failed to change the branch: {err:#}"));
        let mut graph_id = None;
        let mut name = branch_name;
        if let Some(new_name) = &params.name {
            let new_name = new_name.trim();
            if new_name.is_empty() {
                return Err(invalid_request("a branch name cannot be empty".to_string()));
            }
            graph_id = store
                .name_thread_branch(&thread_id, new_name)
                .await
                .map_err(write_error)?;
            name = Some(new_name.to_string());
        }
        if let Some(archived) = params.archived {
            store
                .set_branch_archived(&thread_id, archived)
                .await
                .map_err(write_error)?;
        }
        if let Some(purpose) = &params.purpose {
            store
                .set_branch_purpose(&thread_id, purpose)
                .await
                .map_err(write_error)?;
            // The branch's chat learns what it is for, when it runs here.
            if !purpose.trim().is_empty()
                && let Ok(id) = ThreadId::from_string(&thread_id)
                && let Ok(thread) = self.thread_manager.get_thread(id).await
            {
                let note = branch_boundary_note(
                    name.as_deref().unwrap_or("this branch"),
                    purpose,
                    parent_name.as_deref(),
                );
                if let Err(err) = thread.inject_response_items(vec![note]).await {
                    tracing::warn!("could not tell branch {thread_id} its purpose: {err}");
                }
            }
        }
        let graph_id = match graph_id {
            Some(graph_id) => Some(graph_id),
            None => store
                .branch_of_thread(&thread_id)
                .await
                .map_err(read_error)?
                .map(|(graph_id, _)| graph_id),
        };
        if let Some(graph_id) = graph_id {
            self.notify_graph_updated(params.thread_id, graph_id).await;
        }
        Ok(Some(ContextGraphBranchUpdateResponse {}.into()))
    }

    async fn context_graph_branch_read(
        &self,
        params: ContextGraphBranchReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let tree = self.branch_tree_of(&params.thread_id).await?;
        let row = tree
            .find(&params.branch)
            .ok_or_else(|| invalid_request(format!("no branch named {}", params.branch)))?;
        let turns = tree
            .turns(&row.name)
            .map_err(|err| invalid_request(err.to_string()))?
            .into_iter()
            .map(|turn| ContextGraphTurn {
                number: to_u32(turn.number),
                turn_id: turn.turn_id,
                inherited: turn.inherited,
                title: turn.title,
                blocks: turn.blocks.iter().map(block).collect(),
            })
            .collect();
        Ok(Some(
            ContextGraphBranchReadResponse {
                branch_id: row.branch.to_string(),
                branch_name: row.name,
                turns,
            }
            .into(),
        ))
    }

    async fn context_graph_block_read(
        &self,
        params: ContextGraphBlockReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let block = thread
            .read_context_block(&params.block_id)
            .await
            .map_err(|err| invalid_request(format!("{err:#}")))?;
        Ok(Some(
            ContextGraphBlockReadResponse {
                block_id: params.block_id.trim().to_string(),
                branch_name: block.branch_name,
                kind: kind_name(block.kind).to_string(),
                text: block.text,
            }
            .into(),
        ))
    }

    async fn context_graph_search(
        &self,
        params: ContextGraphSearchParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let tree = self.branch_tree_of(&params.thread_id).await?;
        let limit =
            usize::try_from(params.limit.unwrap_or(DEFAULT_SEARCH_LIMIT)).unwrap_or(usize::MAX);
        let hits = tree
            .search(&params.query, limit)
            .into_iter()
            .map(|hit| ContextGraphSearchHit {
                branch_id: hit.branch.to_string(),
                branch_name: tree.branch_name(&hit.branch),
                turn: to_u32(hit.turn),
                block: block(&hit.block),
                summary: hit.summary,
            })
            .collect();
        Ok(Some(ContextGraphSearchResponse { hits }.into()))
    }

    async fn context_graph_recall(
        &self,
        params: ContextGraphRecallParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        let view = match params
            .question
            .as_deref()
            .map(str::trim)
            .filter(|question| !question.is_empty())
        {
            Some(question) => {
                let preview = thread
                    .preview_context_recall(question)
                    .await
                    .map_err(|err| invalid_request(format!("recall failed: {err:#}")))?;
                Some(ContextGraphView {
                    question: question.to_string(),
                    selector: preview.selector,
                    blocks: to_u32(preview.entries.len()),
                    tokens: preview.tokens,
                    budget_tokens: preview.budget_tokens,
                    latency_ms: preview.latency_ms,
                    entries: preview
                        .entries
                        .into_iter()
                        .map(|entry| ContextGraphViewEntry {
                            block_id: entry.block_id,
                            title: entry.title,
                            presentation: entry.presentation,
                            reason: entry.reason,
                            tokens: entry.tokens,
                            note: entry.note,
                        })
                        .collect(),
                })
            }
            None => None,
        };
        if params.rebuild {
            thread
                .request_context_recall()
                .await
                .map_err(|err| invalid_request(format!("{err:#}")))?;
        }
        Ok(Some(ContextGraphRecallResponse { view }.into()))
    }

    async fn context_graph_branch_preview(
        &self,
        params: ContextGraphBranchPreviewParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.context_graph_store().await?;
        let tree = store
            .branch_tree(&params.thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to read the context graph: {err:#}")))?
            .ok_or_else(|| invalid_request("this thread is not recorded yet".to_string()))?;
        let row = tree
            .find(params.branch.trim())
            .ok_or_else(|| invalid_request(format!("no branch named {}", params.branch)))?;
        let response = match tree.plan_merge(&row.name, MergeMode::Fold) {
            Ok(plan) => ContextGraphBranchPreviewResponse {
                branch_name: row.name,
                turns: u32::try_from(plan.turns).unwrap_or(u32::MAX),
                summary: Some(plan.message),
            },
            Err(MergeError::NothingToMerge(_)) => ContextGraphBranchPreviewResponse {
                branch_name: row.name,
                turns: 0,
                summary: None,
            },
            Err(err) => return Err(invalid_request(err.to_string())),
        };
        Ok(Some(response.into()))
    }

    async fn context_graph_views_read(
        &self,
        params: ContextGraphViewsReadParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let store = self.context_graph_store().await?;
        let Some((graph_id, branch)) = store
            .branch_of_thread(&params.thread_id)
            .await
            .map_err(|err| internal_error(format!("failed to read the context graph: {err:#}")))?
        else {
            return Ok(Some(
                ContextGraphViewsReadResponse {
                    views: Vec::new(),
                    feedback: None,
                }
                .into(),
            ));
        };
        let limit =
            usize::try_from(params.limit.unwrap_or(DEFAULT_VIEW_LIMIT)).unwrap_or(usize::MAX);
        let cards = store
            .load_cards(&graph_id)
            .await
            .map_err(read_error)?
            .into_iter()
            .map(|card| (card.block_id.clone(), card.title))
            .collect::<std::collections::HashMap<_, _>>();
        let feedback = store
            .recall_feedback_counts(&graph_id, &branch.id)
            .await
            .map_err(read_error)?;
        let views = store
            .load_views(&graph_id)
            .await
            .map_err(|err| internal_error(format!("failed to read recalled views: {err:#}")))?
            .into_iter()
            .rev()
            .filter(|record| record.branch_id == branch.id.as_str())
            .take(limit)
            .map(|record| ContextGraphView {
                question: record.question,
                selector: record.selector,
                blocks: to_u32(record.view.entries.len()),
                tokens: record.view.tokens,
                budget_tokens: record.view.budget_tokens,
                latency_ms: record.stats.latency_ms,
                entries: record
                    .view
                    .entries
                    .iter()
                    .map(|entry| ContextGraphViewEntry {
                        block_id: entry.block_id.to_string(),
                        title: cards.get(&entry.block_id).cloned().unwrap_or_default(),
                        presentation: entry.presentation.name().to_string(),
                        reason: entry.reason.name().to_string(),
                        tokens: entry.tokens,
                        note: entry.note.clone(),
                    })
                    .collect(),
            })
            .collect();
        Ok(Some(
            ContextGraphViewsReadResponse {
                views,
                feedback: Some(ContextGraphRecallFeedback {
                    expanded: feedback.expanded,
                    missed: feedback.missed,
                    requested: feedback.requested,
                }),
            }
            .into(),
        ))
    }
}
