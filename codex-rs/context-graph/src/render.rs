//! Text renderings of a context graph for inspection.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::BlockKind;
use crate::BranchId;
use crate::ContextGraph;

/// Number of blocks first recorded on each branch.
pub fn owned_block_counts(graph: &ContextGraph) -> BTreeMap<BranchId, usize> {
    let mut counts = BTreeMap::new();
    for node in graph.nodes() {
        *counts.entry(node.owner.clone()).or_default() += 1;
    }
    counts
}

/// Number of blocks of each kind in the whole graph.
pub fn kind_counts(graph: &ContextGraph) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for node in graph.nodes() {
        *counts
            .entry(kind_name(node.block.kind).to_string())
            .or_default() += 1;
    }
    counts
}

/// Mermaid flowchart with one node per branch and one edge per fork.
pub fn branch_mermaid(graph: &ContextGraph) -> String {
    let owned = owned_block_counts(graph);
    let branches: Vec<_> = graph.branches().collect();
    let node_id = |id: &BranchId| {
        branches
            .iter()
            .position(|branch| &branch.id == id)
            .map(|index| format!("b{index}"))
    };

    let mut out = String::from("flowchart LR\n");
    for (index, branch) in branches.iter().enumerate() {
        let blocks = owned.get(&branch.id).copied().unwrap_or_default();
        let label = escape(&branch.name);
        let _ = writeln!(out, "    b{index}[\"{label} · {blocks} blocks\"]");
    }
    for branch in &branches {
        let (Some(parent), Some(child)) = (
            branch.parent.as_ref().and_then(&node_id),
            node_id(&branch.id),
        ) else {
            continue;
        };
        match &branch.fork_point {
            Some(fork_point) => {
                let kind = graph
                    .node(fork_point)
                    .map_or("unknown", |node| kind_name(node.block.kind));
                let _ = writeln!(out, "    {parent} -- \"after {kind}\" --> {child}");
            }
            None => {
                let _ = writeln!(out, "    {parent} --> {child}");
            }
        }
    }
    out
}

pub fn kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::UserMessage => "user_message",
        BlockKind::AssistantMessage => "assistant_message",
        BlockKind::Reasoning => "reasoning",
        BlockKind::ToolExchange => "tool_exchange",
        BlockKind::InterAgentMessage => "inter_agent_message",
        BlockKind::ContextFragment => "context_fragment",
        BlockKind::Compaction => "compaction",
    }
}

fn escape(label: &str) -> String {
    label.replace('"', "'")
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
