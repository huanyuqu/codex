use serde::Deserialize;
use serde::Serialize;

use crate::BlockId;

/// What a context block contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// A message written by the user.
    UserMessage,
    /// An assistant message, with the reasoning items that preceded it in the same response.
    AssistantMessage,
    /// Reasoning that no message or tool call followed before the turn ended.
    Reasoning,
    /// One tool call together with all of its outputs.
    ToolExchange,
    /// A message received from another agent.
    InterAgentMessage,
    /// Harness-injected context such as environment details, instructions and tool catalogs.
    ContextFragment,
    /// A compaction summary or an opaque compaction checkpoint.
    Compaction,
    /// The note that merged another branch into this one: a summary of the branch's work, or a
    /// pointer to its history.
    Merge,
}

/// Prefix of the item IDs of merge notes, which makes them merge blocks.
pub const MERGE_ITEM_PREFIX: &str = "cgmerge_";

impl BlockKind {
    /// Whether the harness regenerates this block instead of recalling the recorded copy.
    pub fn is_regenerated_by_harness(self) -> bool {
        matches!(self, Self::ContextFragment)
    }
}

/// A sealed context block. Sealed blocks never change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockId,
    pub kind: BlockKind,
    /// Stable response item IDs, or content-derived keys for items without one, in order.
    pub item_keys: Vec<String>,
    /// Turn that produced the block, when the items record it.
    pub turn_id: Option<String>,
    /// Position of the first item in the source stream.
    pub first_ordinal: u64,
    /// Position of the last item in the source stream.
    pub last_ordinal: u64,
    /// Serialized size of the items, used for budgeting before token counts are known.
    pub approx_bytes: u64,
    /// False when a tool call was sealed without its output, or an output without its call.
    pub complete: bool,
}
