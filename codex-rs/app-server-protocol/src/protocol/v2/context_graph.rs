//! A conversation's context graph: its branches, merges between them, and recalled views.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphReadParams {
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphReadResponse {
    /// The graph the thread records into; null when it is not recorded yet.
    pub graph_id: Option<String>,
    /// Branches in tree order: each root, then its forks depth first.
    pub branches: Vec<ContextGraphBranch>,
    pub merges: Vec<ContextGraphMerge>,
    /// How the thread manages its history; null when the thread is not loaded.
    pub mode: Option<ContextGraphMode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ContextGraphMode {
    /// The whole history, compacted into a summary when the context window fills up.
    Full,
    /// The whole history until it fills the soft limit, then a recalled history.
    Auto,
    /// A recalled history for every turn.
    Recall,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphModeSetParams {
    pub thread_id: String,
    pub mode: ContextGraphMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphModeSetResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ContextGraphBranchStatus {
    Active,
    /// History removed from a thread by rollback.
    RolledBack,
    Merged,
    Archived,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranch {
    pub branch_id: String,
    pub name: String,
    /// The thread that runs the branch; null for history no thread runs.
    pub thread_id: Option<String>,
    pub parent_branch_id: Option<String>,
    /// How many forks lie between the branch and its root.
    pub depth: u32,
    pub status: ContextGraphBranchStatus,
    /// User turns the branch added after it was forked.
    pub turns: u32,
    /// Title of the branch's latest request.
    pub last_request: Option<String>,
    /// What the branch is about, once the shadow agent has described it.
    #[serde(default)]
    pub summary: Option<String>,
    /// Whether this is the branch of the thread the graph was read for.
    pub current: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub enum ContextGraphMergeMode {
    /// A summary of the branch's work enters the target thread's history.
    Fold,
    /// The branch's history becomes part of what the target thread's recall chooses from.
    Reference,
    /// A model writes a merge note from the branch's history.
    Synthesize,
    /// The branch's history is copied into the target thread's.
    Inline,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphMerge {
    pub target_branch_id: String,
    pub source_branch_id: String,
    pub mode: ContextGraphMergeMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphMergeParams {
    /// The thread to merge into.
    pub thread_id: String,
    /// The branch to merge: a branch ID, a branch name or a thread ID.
    pub branch: String,
    pub mode: ContextGraphMergeMode,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphMergeResponse {
    pub branch_id: String,
    pub branch_name: String,
    /// User turns the merged branch added after it diverged.
    pub turns: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchRenameParams {
    /// The thread whose branch to rename.
    pub thread_id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchRenameResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchPreviewParams {
    /// The thread the branch would be merged into.
    pub thread_id: String,
    /// Branch name, branch ID or thread ID of the branch to preview.
    pub branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchPreviewResponse {
    pub branch_name: String,
    /// User turns the branch has that the thread lacks.
    pub turns: u32,
    /// What a fold of the branch into the thread would add; `None` when the branch has no work
    /// the thread lacks.
    pub summary: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphViewsReadParams {
    pub thread_id: String,
    /// Most recent views to return. Defaults to 20.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphViewsReadResponse {
    /// Views recall built for the thread's branch, most recent first.
    pub views: Vec<ContextGraphView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphView {
    /// The request recall chose context for.
    pub question: String,
    /// The model that screened the history.
    pub selector: String,
    /// Blocks the view shows.
    pub blocks: u32,
    /// Estimated tokens of the view.
    pub tokens: u32,
    pub budget_tokens: u32,
    pub latency_ms: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphUpdatedNotification {
    /// The thread whose graph changed, such as by a merge or a rename.
    pub thread_id: String,
    pub graph_id: String,
}
