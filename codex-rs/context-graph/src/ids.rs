use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// Stable identifier of a context block.
///
/// Derived from the key of the block's first item, so the same history always maps to the same
/// block ID, including history that a fork inherited from its source thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockId(String);

impl BlockId {
    pub(crate) fn for_item_key(item_key: &str) -> Self {
        Self(format!("blk_{item_key}"))
    }

    /// Restores an ID that was previously produced by this crate and stored elsewhere.
    pub fn from_stored(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Identifier of a branch in a context graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BranchId(String);

impl BranchId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The branch executed by a thread. Each thread runs exactly one branch.
    pub fn for_thread(thread_id: &str) -> Self {
        Self(format!("br_{thread_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
