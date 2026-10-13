use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const CONTEXT_SEARCH_TOOL_NAME: &str = "context_search";
pub(crate) const CONTEXT_EXPAND_TOOL_NAME: &str = "context_expand";

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
