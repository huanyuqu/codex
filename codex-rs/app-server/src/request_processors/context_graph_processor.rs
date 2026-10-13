//! Requests about a conversation's context graph: its branch tree, merges and renames, and the
//! views recall built.

use codex_app_server_protocol::ContextGraphBranch;
use codex_app_server_protocol::ContextGraphBranchPreviewParams;
use codex_app_server_protocol::ContextGraphBranchPreviewResponse;
use codex_app_server_protocol::ContextGraphBranchRenameParams;
use codex_app_server_protocol::ContextGraphBranchRenameResponse;
use codex_app_server_protocol::ContextGraphBranchStatus;
use codex_app_server_protocol::ContextGraphMerge;
use codex_app_server_protocol::ContextGraphMergeMode;
use codex_app_server_protocol::ContextGraphMergeParams;
use codex_app_server_protocol::ContextGraphMergeResponse;
use codex_app_server_protocol::ContextGraphMode;
use codex_app_server_protocol::ContextGraphModeSetParams;
use codex_app_server_protocol::ContextGraphModeSetResponse;
use codex_app_server_protocol::ContextGraphReadParams;
use codex_app_server_protocol::ContextGraphReadResponse;
use codex_app_server_protocol::ContextGraphUpdatedNotification;
use codex_app_server_protocol::ContextGraphView;
use codex_app_server_protocol::ContextGraphViewsReadParams;
use codex_app_server_protocol::ContextGraphViewsReadResponse;
use codex_config::types::ContextMode;
use codex_context_graph::BranchStatus;
use codex_context_graph_store::LocalContextGraphStore;
use codex_context_graph_store::MergeError;
use codex_context_graph_store::MergeMode;

use super::*;

const DEFAULT_VIEW_LIMIT: u32 = 20;

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

impl TurnRequestProcessor {
    async fn context_graph_store(&self) -> Result<LocalContextGraphStore, JSONRPCErrorError> {
        LocalContextGraphStore::open(&self.config.sqlite)
            .await
            .map_err(|err| internal_error(format!("context graph unavailable: {err:#}")))
    }

    pub(crate) async fn context_graph_read(
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

    pub(crate) async fn context_graph_merge(
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
        let (_, thread) = self.load_thread(&params.thread_id).await?;
        self.ensure_direct_input_allowed(request_id, thread.as_ref())
            .await?;
        let merged = thread
            .merge_context_branch(&params.branch, mode)
            .await
            .map_err(|err| invalid_request(format!("{err:#}")))?;
        self.outgoing
            .send_server_notification(ServerNotification::ContextGraphUpdated(
                ContextGraphUpdatedNotification {
                    thread_id: params.thread_id,
                    graph_id: merged.graph_id,
                },
            ))
            .await;
        Ok(Some(
            ContextGraphMergeResponse {
                branch_id: merged.branch_id,
                branch_name: merged.branch_name,
                turns: to_u32(merged.turns),
            }
            .into(),
        ))
    }

    pub(crate) async fn context_graph_mode_set(
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

    pub(crate) async fn context_graph_branch_rename(
        &self,
        params: ContextGraphBranchRenameParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let name = params.name.trim();
        if name.is_empty() {
            return Err(invalid_request("a branch name cannot be empty".to_string()));
        }
        // A thread that has not recorded yet gets the name once its branch exists.
        let store = self.context_graph_store().await?;
        let graph_id = store
            .name_thread_branch(&params.thread_id, name)
            .await
            .map_err(|err| internal_error(format!("failed to rename the branch: {err:#}")))?;
        if let Some(graph_id) = graph_id {
            self.outgoing
                .send_server_notification(ServerNotification::ContextGraphUpdated(
                    ContextGraphUpdatedNotification {
                        thread_id: params.thread_id,
                        graph_id,
                    },
                ))
                .await;
        }
        Ok(Some(ContextGraphBranchRenameResponse {}.into()))
    }

    pub(crate) async fn context_graph_branch_preview(
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

    pub(crate) async fn context_graph_views_read(
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
                ContextGraphViewsReadResponse { views: Vec::new() }.into(),
            ));
        };
        let limit =
            usize::try_from(params.limit.unwrap_or(DEFAULT_VIEW_LIMIT)).unwrap_or(usize::MAX);
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
            })
            .collect();
        Ok(Some(ContextGraphViewsReadResponse { views }.into()))
    }
}
