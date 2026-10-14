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
    /// What the branch is for, as its creator described it.
    #[serde(default)]
    pub purpose: Option<String>,
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
    /// The branches to merge: branch IDs, branch names or thread IDs. A saved thread outside
    /// the conversation joins it as a branch first.
    pub branches: Vec<String>,
    pub mode: ContextGraphMergeMode,
    /// What the merge is for; a synthesizing model writes the note toward it.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub goal: Option<String>,
    /// Also merge the code of branches that work in their own git worktree.
    #[serde(default)]
    pub code: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphMergeResponse {
    /// The merged branches, in the order they were given.
    pub merged: Vec<ContextGraphMergedBranch>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphMergedBranch {
    pub branch_id: String,
    pub branch_name: String,
    /// User turns the merged branch added after it diverged.
    pub turns: u32,
    /// The code merged from the branch's worktree, when the merge included code.
    pub code: Option<ContextGraphCodeMerge>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphCodeMerge {
    /// The git branch that was merged.
    pub git_branch: String,
    /// The commit that saved the branch's uncommitted work before the merge.
    pub snapshot_commit: Option<String>,
    /// The merge commit; null when the merge stopped at conflicts or had nothing to merge.
    pub merge_commit: Option<String>,
    /// Files left with conflicts, to resolve in the target's worktree.
    pub conflicts: Vec<String>,
    /// Files the merge changed.
    pub files: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchUpdateParams {
    /// The thread whose conversation holds the branch.
    pub thread_id: String,
    /// The branch to change: a branch ID, a branch name or a thread ID. Defaults to the
    /// thread's own branch.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub branch: Option<String>,
    #[serde(default)]
    #[ts(optional = nullable)]
    pub name: Option<String>,
    /// What the branch is for. The branch's chat is told when it is set; an empty string clears
    /// it.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub purpose: Option<String>,
    /// Archive the branch, or restore it. Archived branches keep their history.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub archived: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchUpdateResponse {}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchReadParams {
    pub thread_id: String,
    /// Branch name, branch ID or thread ID of the branch to read.
    pub branch: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBranchReadResponse {
    pub branch_id: String,
    pub branch_name: String,
    /// The turns of the branch's whole history, oldest first.
    pub turns: Vec<ContextGraphTurn>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphTurn {
    /// Position of the turn in the branch's history, counted from 1; 0 for what came before the
    /// first request.
    pub number: u32,
    pub turn_id: Option<String>,
    /// Whether the branch inherited the turn from the branch it was forked from.
    pub inherited: bool,
    /// The request that started the turn.
    pub title: String,
    pub blocks: Vec<ContextGraphBlock>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBlock {
    pub block_id: String,
    /// What the block holds, such as "user", "assistant", "tool" or "merge".
    pub kind: String,
    /// The block card's title.
    pub title: String,
    /// Estimated tokens of the block.
    pub tokens: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBlockReadParams {
    pub thread_id: String,
    pub block_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphBlockReadResponse {
    pub block_id: String,
    /// The branch that recorded the block.
    pub branch_name: String,
    pub kind: String,
    /// The block as text: its messages, tool calls and outputs.
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphSearchParams {
    pub thread_id: String,
    /// Words, file names or symbols to look for in the block cards.
    pub query: String,
    /// Most hits to return. Defaults to 20.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphSearchResponse {
    /// Matching blocks of every branch, best first.
    pub hits: Vec<ContextGraphSearchHit>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphSearchHit {
    pub branch_id: String,
    pub branch_name: String,
    /// The turn of that branch the block belongs to.
    pub turn: u32,
    pub block: ContextGraphBlock,
    pub summary: String,
}

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
    #[serde(default)]
    pub feedback: Option<ContextGraphRecallFeedback>,
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
    /// The blocks the view shows, in history order.
    #[serde(default)]
    pub entries: Vec<ContextGraphViewEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphViewEntry {
    pub block_id: String,
    pub title: String,
    /// How much of the block the view shows: "full", "excerpt" or "summary".
    pub presentation: String,
    /// Why the view shows it: "recent", "taskStart", "selected", "dependency" or "successor".
    pub reason: String,
    pub tokens: u32,
    /// What the selector said about the block.
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphRecallFeedback {
    /// Blocks the model read with `context_expand`.
    pub expanded: u32,
    /// Of those, blocks the recalled window had left out.
    pub missed: u32,
    /// Recalls the user asked for.
    pub requested: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphRecallParams {
    pub thread_id: String,
    /// A request to choose context for without changing the thread: the response shows the
    /// view recall would build.
    #[serde(default)]
    #[ts(optional = nullable)]
    pub question: Option<String>,
    /// Rebuild the thread's history by recall before its next turn.
    #[serde(default)]
    pub rebuild: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphRecallResponse {
    /// The view recall built for `question`, if one was given.
    pub view: Option<ContextGraphView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphUpdatedNotification {
    /// The thread whose graph changed, such as by a merge or a rename.
    pub thread_id: String,
    pub graph_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ContextGraphCardsUpdatedNotification {
    /// The thread whose blocks got new cards.
    pub thread_id: String,
    pub graph_id: String,
    /// How many blocks got cards.
    pub blocks: u32,
}
