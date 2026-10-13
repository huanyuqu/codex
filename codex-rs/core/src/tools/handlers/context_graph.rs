//! Tools that read conversation history recall left out of the context, and that fork and
//! merge branches of the conversation.

use std::time::Duration;

use codex_context_graph::lookup::expand_block;
use codex_context_graph::lookup::expand_turn;
use codex_context_graph::lookup::search_history;
use codex_context_graph::text::READ_BLOCK_CHARS;
use codex_context_graph_store::BranchSnapshot;
use codex_context_graph_store::MergeMode;
use codex_protocol::ThreadId;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::agent::status::is_final;
use crate::context_graph::ContextGraphService;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::context_graph_spec::BRANCH_CREATE_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::BRANCH_MERGE_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::CONTEXT_EXPAND_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::CONTEXT_RECALL_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::CONTEXT_SEARCH_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::create_branch_create_tool;
use crate::tools::handlers::context_graph_spec::create_branch_merge_tool;
use crate::tools::handlers::context_graph_spec::create_context_expand_tool;
use crate::tools::handlers::context_graph_spec::create_context_recall_tool;
use crate::tools::handlers::context_graph_spec::create_context_search_tool;
use crate::tools::handlers::multi_agents::spawn::handle_spawn_agent;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_SEARCH_LIMIT: usize = 30;
/// Characters a whole turn may expand to.
const MAX_TURN_CHARS: usize = 2 * READ_BLOCK_CHARS;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct RecallArgs {
    question: String,
}

#[derive(Deserialize)]
struct ExpandArgs {
    block_id: Option<String>,
    turn: Option<usize>,
}

pub struct ContextSearchHandler;

pub struct ContextExpandHandler;

pub struct ContextRecallHandler;

fn arguments(payload: ToolPayload, tool: &str) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(format!(
            "{tool} handler received unsupported payload"
        ))),
    }
}

fn service(session: &Session) -> Result<&ContextGraphService, FunctionCallError> {
    session.services.context_graph.as_deref().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "The conversation history is not recorded in this session.".to_string(),
        )
    })
}

async fn snapshot(session: &Session) -> Result<BranchSnapshot, FunctionCallError> {
    service(session)?.snapshot().await.map_err(|err| {
        FunctionCallError::RespondToModel(format!("Could not read the history: {err:#}"))
    })
}

fn text_output(text: String) -> Box<dyn ToolOutput> {
    boxed_tool_output(FunctionToolOutput::from_text(text, Some(true)))
}

impl ToolExecutor<ToolInvocation> for ContextSearchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CONTEXT_SEARCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_context_search_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let SearchArgs { query, limit } =
                parse_arguments(&arguments(invocation.payload, CONTEXT_SEARCH_TOOL_NAME)?)?;
            let snapshot = snapshot(&invocation.session).await?;
            let limit = limit
                .unwrap_or(DEFAULT_SEARCH_LIMIT)
                .clamp(1, MAX_SEARCH_LIMIT);
            Ok(text_output(search_history(
                &snapshot.history(/*until_turn*/ None),
                &query,
                limit,
            )))
        })
    }
}

impl CoreToolRuntime for ContextSearchHandler {}

impl ToolExecutor<ToolInvocation> for ContextExpandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CONTEXT_EXPAND_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_context_expand_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ExpandArgs { block_id, turn } =
                parse_arguments(&arguments(invocation.payload, CONTEXT_EXPAND_TOOL_NAME)?)?;
            let snapshot = snapshot(&invocation.session).await?;
            let history = snapshot.history(/*until_turn*/ None);
            let text = match (block_id, turn) {
                (Some(block_id), _) => expand_block(&history, block_id.trim())
                    .ok_or_else(|| format!("No block {block_id} in this conversation.")),
                (None, Some(turn)) => expand_turn(&history, turn, MAX_TURN_CHARS)
                    .ok_or_else(|| format!("No turn {turn} in this conversation.")),
                (None, None) => Err("Give block_id or turn.".to_string()),
            }
            .map_err(FunctionCallError::RespondToModel)?;
            Ok(text_output(text))
        })
    }
}

impl CoreToolRuntime for ContextExpandHandler {}

impl ToolExecutor<ToolInvocation> for ContextRecallHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(CONTEXT_RECALL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_context_recall_tool()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let RecallArgs { question } =
                parse_arguments(&arguments(invocation.payload, CONTEXT_RECALL_TOOL_NAME)?)?;
            let text = service(&invocation.session)?
                .recall_for_tool(&question)
                .await
                .map_err(|err| {
                    FunctionCallError::RespondToModel(format!("Recall failed: {err:#}"))
                })?;
            Ok(text_output(text))
        })
    }
}

impl CoreToolRuntime for ContextRecallHandler {}

/// How long `branch_merge` waits for a working branch by default, and at most.
const DEFAULT_BRANCH_WAIT_SECS: u64 = 600;
const MAX_BRANCH_WAIT_SECS: u64 = 3600;

#[derive(Deserialize)]
struct BranchCreateArgs {
    name: String,
    task: String,
}

#[derive(Deserialize)]
struct BranchMergeArgs {
    branch: String,
    mode: Option<String>,
    wait_seconds: Option<u64>,
}

pub struct BranchCreateHandler;

pub struct BranchMergeHandler;

impl ToolExecutor<ToolInvocation> for BranchCreateHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(BRANCH_CREATE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_branch_create_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = arguments(invocation.payload.clone(), BRANCH_CREATE_TOOL_NAME)?;
            let BranchCreateArgs { name, task } = parse_arguments(&arguments)?;
            let name = name.trim().to_string();
            if name.is_empty() || task.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "Give the branch a name and a task.".to_string(),
                ));
            }
            let session = std::sync::Arc::clone(&invocation.session);
            let service = service(&session)?;
            if let Ok(tree) = service.branch_tree().await
                && tree.find(&name).is_some()
            {
                return Err(FunctionCallError::RespondToModel(format!(
                    "A branch named {name} already exists; choose another name."
                )));
            }
            // A full-history fork of this conversation joins its graph as a branch.
            let spawn = ToolInvocation {
                payload: ToolPayload::Function {
                    arguments: json!({ "message": task, "fork_context": true }).to_string(),
                },
                ..invocation
            };
            let spawned = handle_spawn_agent(spawn).await?;
            let thread_id = spawned.agent_id().to_string();
            if let Err(err) = service.name_thread_branch(&thread_id, &name).await {
                tracing::warn!("could not name branch {name}: {err:#}");
            }
            Ok(text_output(format!(
                "Started branch {name} (agent {thread_id}). It works on its own from the history \
                 so far; call branch_merge with branch \"{name}\" when you need its results."
            )))
        })
    }
}

impl CoreToolRuntime for BranchCreateHandler {}

impl ToolExecutor<ToolInvocation> for BranchMergeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(BRANCH_MERGE_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_branch_merge_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let BranchMergeArgs {
                branch,
                mode,
                wait_seconds,
            } = parse_arguments(&arguments(invocation.payload, BRANCH_MERGE_TOOL_NAME)?)?;
            let mode = match mode.as_deref().map(str::trim) {
                None | Some("fold") => MergeMode::Fold,
                Some("reference") => MergeMode::Reference,
                Some("synthesize") => MergeMode::Synthesize,
                Some("inline") => MergeMode::Inline,
                Some(other) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Unknown merge mode {other}; use fold, reference, synthesize or inline."
                    )));
                }
            };
            let session = invocation.session;
            let service = service(&session)?;
            let tree = service.branch_tree().await.map_err(|err| {
                FunctionCallError::RespondToModel(format!("Could not read the branches: {err:#}"))
            })?;
            let row = tree.find(branch.trim()).ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "No branch {branch}. Branches: {}.",
                    tree.rows()
                        .iter()
                        .filter(|row| !row.current)
                        .map(|row| row.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let wait = Duration::from_secs(
                wait_seconds
                    .unwrap_or(DEFAULT_BRANCH_WAIT_SECS)
                    .min(MAX_BRANCH_WAIT_SECS),
            );
            if let Some(thread_id) = row
                .thread_id
                .as_deref()
                .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
                && !wait_for_branch(&session, thread_id, wait).await
            {
                return Ok(text_output(format!(
                    "Branch {} is still working. Continue with something else and call \
                     branch_merge again later.",
                    row.name
                )));
            }
            let merge = service
                .prepare_merge(&row.name, mode)
                .await
                .map_err(|err| FunctionCallError::RespondToModel(format!("{err:#}")))?;
            // The note enters the history right after this call's output.
            if session
                .inject_if_running(vec![merge.note.clone()])
                .await
                .is_err()
            {
                return Err(FunctionCallError::RespondToModel(
                    "The turn ended before the merge could be added.".to_string(),
                ));
            }
            crate::context_graph::commit_merge(&session, &merge).await;
            let how = match mode {
                MergeMode::Fold => "a summary of its work",
                MergeMode::Reference => "a reference recall can draw on",
                MergeMode::Synthesize => "a merge note",
                MergeMode::Inline => "its history",
            };
            Ok(text_output(format!(
                "Merged branch {} ({} turns) as {how}, which follows in the conversation.",
                merge.branch_name, merge.turns
            )))
        })
    }
}

impl CoreToolRuntime for BranchMergeHandler {}

/// Waits up to `wait` for the agent running the branch's thread to finish its work. A thread no
/// agent runs, such as a chat the user forked, counts as finished.
async fn wait_for_branch(session: &Session, thread_id: ThreadId, wait: Duration) -> bool {
    let control = session
        .services
        .local_agent_runtime
        .control(session.session_id());
    let Ok(mut updates) = control.subscribe_status(thread_id).await else {
        return true;
    };
    let finished = async {
        while let Some(update) = updates.next().await {
            match update {
                Ok(info) => {
                    if info.status().is_none_or(is_final) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    };
    tokio::time::timeout(wait, finished).await.is_ok()
}
