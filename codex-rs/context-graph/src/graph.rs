//! Append-only DAG of context blocks with named branch heads.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::Block;
use crate::BlockId;
use crate::BlockKind;
use crate::BranchId;

/// Serialized graph format; the earlier multi-session merge archives used
/// `codex-conversation-graph-v1`.
pub const GRAPH_FORMAT: &str = "codex-context-graph-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchStatus {
    Active,
    /// History removed from a thread by rollback, kept as an abandoned branch.
    RolledBack,
    Merged,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub id: BranchId,
    pub name: String,
    /// Thread that executes this branch, when there is one.
    pub thread_id: Option<String>,
    pub parent: Option<BranchId>,
    /// Last block shared with the parent branch.
    pub fork_point: Option<BlockId>,
    pub head: Option<BlockId>,
    pub status: BranchStatus,
}

impl Branch {
    /// An active branch with no history yet.
    pub fn new(id: BranchId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            thread_id: None,
            parent: None,
            fork_point: None,
            head: None,
            status: BranchStatus::Active,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockNode {
    pub block: Block,
    /// Parent blocks. The first is the primary parent; a root block has none.
    pub parents: Vec<BlockId>,
    /// Branch on which the block was first recorded.
    pub owner: BranchId,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphError {
    #[error("unknown branch {0}")]
    UnknownBranch(BranchId),
    #[error("branch {0} already exists")]
    DuplicateBranch(BranchId),
    #[error("unknown block {0}")]
    UnknownBlock(BlockId),
    #[error("block {block} has parent {parent} that is missing or recorded after it")]
    InvalidParent { block: BlockId, parent: BlockId },
}

/// What [`ContextGraph::append`] did with a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// A new block was recorded at the branch head.
    Added,
    /// The block already follows the branch head, for example history inherited by a fork.
    /// The head moved forward without recording anything.
    Inherited,
    /// The block was recorded earlier but does not follow the head, for example a user message
    /// that compaction kept in its replacement history. Nothing changed.
    Reincluded,
}

/// Portable form of a graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphExport {
    pub format: String,
    pub branches: Vec<Branch>,
    /// Blocks in the order they were recorded; parents always precede children.
    pub blocks: Vec<BlockNode>,
}

/// Append-only DAG of context blocks.
#[derive(Debug, Default, Clone)]
pub struct ContextGraph {
    order: Vec<BlockId>,
    nodes: HashMap<BlockId, BlockNode>,
    branches: BTreeMap<BranchId, Branch>,
}

impl ContextGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_branch(&mut self, branch: Branch) -> Result<(), GraphError> {
        if self.branches.contains_key(&branch.id) {
            return Err(GraphError::DuplicateBranch(branch.id));
        }
        if let Some(parent) = &branch.parent
            && !self.branches.contains_key(parent)
        {
            return Err(GraphError::UnknownBranch(parent.clone()));
        }
        for block in branch.fork_point.iter().chain(branch.head.iter()) {
            if !self.nodes.contains_key(block) {
                return Err(GraphError::UnknownBlock(block.clone()));
            }
        }
        self.branches.insert(branch.id.clone(), branch);
        Ok(())
    }

    pub fn branch(&self, id: &BranchId) -> Option<&Branch> {
        self.branches.get(id)
    }

    pub fn branches(&self) -> impl Iterator<Item = &Branch> {
        self.branches.values()
    }

    pub fn node(&self, id: &BlockId) -> Option<&BlockNode> {
        self.nodes.get(id)
    }

    /// Blocks in the order they were recorded.
    pub fn nodes(&self) -> impl Iterator<Item = &BlockNode> {
        self.order.iter().filter_map(|id| self.nodes.get(id))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Records `block` at the head of `branch_id`, or moves the head over a block the branch
    /// inherited.
    ///
    /// The first new block on a branch whose head belongs to another branch marks the fork point.
    /// Replay parents before forks so the shared prefix is owned by the parent.
    pub fn append(
        &mut self,
        branch_id: &BranchId,
        block: Block,
    ) -> Result<AppendOutcome, GraphError> {
        let head = self
            .branches
            .get(branch_id)
            .ok_or_else(|| GraphError::UnknownBranch(branch_id.clone()))?
            .head
            .clone();

        if let Some(existing) = self.nodes.get(&block.id) {
            if existing.parents.first() != head.as_ref() {
                return Ok(AppendOutcome::Reincluded);
            }
            if let Some(branch) = self.branches.get_mut(branch_id) {
                branch.head = Some(block.id);
            }
            return Ok(AppendOutcome::Inherited);
        }

        let head_owner = head
            .as_ref()
            .and_then(|head| self.nodes.get(head))
            .map(|node| node.owner.clone());
        let id = block.id.clone();
        self.nodes.insert(
            id.clone(),
            BlockNode {
                block,
                parents: head.iter().cloned().collect(),
                owner: branch_id.clone(),
            },
        );
        self.order.push(id.clone());
        if let Some(branch) = self.branches.get_mut(branch_id) {
            if branch.fork_point.is_none()
                && let Some(owner) = head_owner.filter(|owner| owner != branch_id)
            {
                branch.fork_point = branch.head.clone();
                if branch.parent.is_none() {
                    branch.parent = Some(owner);
                }
            }
            branch.head = Some(id);
        }
        Ok(AppendOutcome::Added)
    }

    /// Moves `branch_id` back before its last `num_user_turns` user messages.
    ///
    /// The removed blocks stay in the graph under a new rolled-back branch, which is returned.
    pub fn rollback(
        &mut self,
        branch_id: &BranchId,
        num_user_turns: u32,
    ) -> Result<Option<BranchId>, GraphError> {
        let branch = self
            .branches
            .get(branch_id)
            .ok_or_else(|| GraphError::UnknownBranch(branch_id.clone()))?
            .clone();
        let Some(old_head) = branch.head.clone() else {
            return Ok(None);
        };
        if num_user_turns == 0 {
            return Ok(None);
        }

        let mut remaining = num_user_turns;
        let mut new_head = Some(old_head.clone());
        let mut cursor = Some(old_head.clone());
        while let Some(id) = cursor {
            let node = self
                .nodes
                .get(&id)
                .ok_or_else(|| GraphError::UnknownBlock(id.clone()))?;
            let parent = node.parents.first().cloned();
            if node.block.kind == BlockKind::UserMessage {
                remaining -= 1;
                if remaining == 0 {
                    new_head = parent;
                    break;
                }
            }
            if parent.is_none() {
                // Fewer user turns than requested: drop everything.
                new_head = None;
            }
            cursor = parent;
        }
        if new_head.as_ref() == Some(&old_head) {
            return Ok(None);
        }

        let index = self
            .branches
            .values()
            .filter(|candidate| {
                candidate.parent.as_ref() == Some(branch_id)
                    && candidate.status == BranchStatus::RolledBack
            })
            .count()
            + 1;
        let abandoned = Branch {
            id: BranchId::new(format!("{branch_id}~rollback-{index}")),
            name: format!("{} (rolled back {index})", branch.name),
            thread_id: branch.thread_id,
            parent: Some(branch_id.clone()),
            fork_point: new_head.clone(),
            head: Some(old_head),
            status: BranchStatus::RolledBack,
        };
        let abandoned_id = abandoned.id.clone();
        self.branches.insert(abandoned_id.clone(), abandoned);
        if let Some(branch) = self.branches.get_mut(branch_id) {
            branch.head = new_head;
        }
        Ok(Some(abandoned_id))
    }

    /// Blocks from the root to `head` along primary parents.
    pub fn primary_path(&self, head: &BlockId) -> Result<Vec<BlockId>, GraphError> {
        let mut path = Vec::new();
        let mut cursor = Some(head.clone());
        while let Some(id) = cursor {
            let node = self
                .nodes
                .get(&id)
                .ok_or_else(|| GraphError::UnknownBlock(id.clone()))?;
            cursor = node.parents.first().cloned();
            path.push(id);
        }
        path.reverse();
        Ok(path)
    }

    /// Every block reachable from `head` through any parent, including `head`.
    pub fn ancestors(&self, head: &BlockId) -> Result<HashSet<BlockId>, GraphError> {
        let mut found = HashSet::new();
        let mut pending = vec![head.clone()];
        while let Some(id) = pending.pop() {
            if !found.insert(id.clone()) {
                continue;
            }
            let node = self
                .nodes
                .get(&id)
                .ok_or_else(|| GraphError::UnknownBlock(id.clone()))?;
            pending.extend(node.parents.iter().cloned());
        }
        Ok(found)
    }

    /// The most recently recorded block that both heads can reach.
    pub fn merge_base(&self, a: &BlockId, b: &BlockId) -> Result<Option<BlockId>, GraphError> {
        let from_a = self.ancestors(a)?;
        let from_b = self.ancestors(b)?;
        Ok(self
            .order
            .iter()
            .rev()
            .find(|id| from_a.contains(*id) && from_b.contains(*id))
            .cloned())
    }

    /// Checks that parents precede children and that branch references resolve.
    pub fn validate(&self) -> Result<(), GraphError> {
        let positions: HashMap<&BlockId, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        for (index, id) in self.order.iter().enumerate() {
            let node = self
                .nodes
                .get(id)
                .ok_or_else(|| GraphError::UnknownBlock(id.clone()))?;
            for parent in &node.parents {
                if positions
                    .get(parent)
                    .is_none_or(|position| *position >= index)
                {
                    return Err(GraphError::InvalidParent {
                        block: id.clone(),
                        parent: parent.clone(),
                    });
                }
            }
        }
        for branch in self.branches.values() {
            if let Some(parent) = &branch.parent
                && !self.branches.contains_key(parent)
            {
                return Err(GraphError::UnknownBranch(parent.clone()));
            }
            for block in branch.fork_point.iter().chain(branch.head.iter()) {
                if !self.nodes.contains_key(block) {
                    return Err(GraphError::UnknownBlock(block.clone()));
                }
            }
        }
        Ok(())
    }

    pub fn export(&self) -> GraphExport {
        GraphExport {
            format: GRAPH_FORMAT.to_string(),
            branches: self.branches.values().cloned().collect(),
            blocks: self.nodes().cloned().collect(),
        }
    }

    /// Rebuilds a graph from its export, checking every invariant.
    pub fn import(export: GraphExport) -> Result<Self, GraphError> {
        let mut graph = Self::default();
        for node in export.blocks {
            let id = node.block.id.clone();
            graph.order.push(id.clone());
            graph.nodes.insert(id, node);
        }
        for branch in export.branches {
            graph.branches.insert(branch.id.clone(), branch);
        }
        graph.validate()?;
        Ok(graph)
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
