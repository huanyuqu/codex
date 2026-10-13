use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const CONTEXT_SEARCH_TOOL_NAME: &str = "context_search";
pub(crate) const CONTEXT_EXPAND_TOOL_NAME: &str = "context_expand";
pub(crate) const CONTEXT_RECALL_TOOL_NAME: &str = "context_recall";
pub(crate) const BRANCH_CREATE_TOOL_NAME: &str = "branch_create";
pub(crate) const BRANCH_MERGE_TOOL_NAME: &str = "branch_merge";

pub fn create_branch_create_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "name".to_string(),
            JsonSchema::string(Some(
                "Short name for the branch, such as \"try-sqlite\".".to_string(),
            )),
        ),
        (
            "task".to_string(),
            JsonSchema::string(Some(
                "What the branch should do, as a request to an agent that knows this whole \
                 conversation."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: BRANCH_CREATE_TOOL_NAME.to_string(),
        description: "Fork this conversation into a branch that works on a task on its own, with \
                      the whole history so far, while you continue. Use it to try an approach \
                      you may discard, or to explore something without filling this \
                      conversation. Branches that share the working directory take turns \
                      changing files. Bring the branch's results back with branch_merge."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["name".to_string(), "task".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_branch_merge_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "branch".to_string(),
            JsonSchema::string(Some("Name of the branch to merge.".to_string())),
        ),
        (
            "mode".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("fold"),
                    json!("reference"),
                    json!("synthesize"),
                    json!("inline"),
                ],
                Some(
                    "fold (default) adds a summary of the branch's work; reference lets recall \
                     draw on the branch's history when needed; synthesize adds a merge note a \
                     model writes; inline copies the branch's history."
                        .to_string(),
                ),
            ),
        ),
        (
            "wait_seconds".to_string(),
            JsonSchema::integer(Some(
                "How long to wait for a branch that is still working before giving up. \
                 Defaults to 600."
                    .to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: BRANCH_MERGE_TOOL_NAME.to_string(),
        description: "Merge a branch of this conversation, such as one branch_create started, \
                      into it. Waits for the branch to finish its work first."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["branch".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_context_recall_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "question".to_string(),
        JsonSchema::string(Some(
            "What you need from earlier in the conversation, such as \"why the cache test \
             failed\" or \"what the user said about the public API\"."
                .to_string(),
        )),
    )]);
    ToolSpec::Function(ResponsesApiTool {
        name: CONTEXT_RECALL_TOOL_NAME.to_string(),
        description: "Ask for the parts of this conversation's history, including parts \
                      omitted from your context, that a question needs. A model reads the whole \
                      history and returns the relevant blocks. Slower than context_search, but \
                      finds what words alone do not."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["question".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_context_search_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "query".to_string(),
            JsonSchema::string(Some(
                "Words to look for, such as a file, function, error or topic.".to_string(),
            )),
        ),
        (
            "limit".to_string(),
            JsonSchema::integer(Some("Most blocks to return. Defaults to 8.".to_string())),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: CONTEXT_SEARCH_TOOL_NAME.to_string(),
        description: "Search this conversation's whole history, including parts omitted from \
                      your context, for blocks matching a query. Returns one line per block with \
                      its ID, turn and summary; read a block with context_expand."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["query".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn create_context_expand_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "block_id".to_string(),
            JsonSchema::string(Some(
                "ID of a block, as context_search returns it.".to_string(),
            )),
        ),
        (
            "turn".to_string(),
            JsonSchema::integer(Some(
                "Number of a turn, as the map of earlier turns lists it.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: CONTEXT_EXPAND_TOOL_NAME.to_string(),
        description: "Read a block, or all blocks of a turn, of this conversation's history in \
                      full, including parts omitted from your context. Give block_id or turn."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    })
}
