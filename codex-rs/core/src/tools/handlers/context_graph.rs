//! Tools that read conversation history recall left out of the context.

use codex_context_graph::lookup::expand_block;
use codex_context_graph::lookup::expand_turn;
use codex_context_graph::lookup::search_history;
use codex_context_graph::text::READ_BLOCK_CHARS;
use codex_context_graph_store::BranchSnapshot;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::context_graph_spec::CONTEXT_EXPAND_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::CONTEXT_SEARCH_TOOL_NAME;
use crate::tools::handlers::context_graph_spec::create_context_expand_tool;
use crate::tools::handlers::context_graph_spec::create_context_search_tool;
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
struct ExpandArgs {
    block_id: Option<String>,
    turn: Option<usize>,
}

pub struct ContextSearchHandler;

pub struct ContextExpandHandler;

fn arguments(payload: ToolPayload, tool: &str) -> Result<String, FunctionCallError> {
    match payload {
        ToolPayload::Function { arguments } => Ok(arguments),
        _ => Err(FunctionCallError::RespondToModel(format!(
            "{tool} handler received unsupported payload"
        ))),
    }
}

async fn snapshot(session: &Session) -> Result<BranchSnapshot, FunctionCallError> {
    let service = session.services.context_graph.as_ref().ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "The conversation history is not recorded in this session.".to_string(),
        )
    })?;
    service.live().snapshot().await.map_err(|err| {
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
