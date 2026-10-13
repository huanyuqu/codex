//! The branches of a context graph as a tree, and folding the work of one branch into another.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use crate::BlockId;
use crate::BlockKind;
use crate::Branch;
use crate::BranchId;
use crate::BranchStatus;
use crate::ContextGraph;
use crate::GraphError;
use crate::card::BlockCard;
use crate::card::ClaimKind;
use crate::text::truncate;

const MAX_FOLD_CLAIMS: usize = 15;
const RESULT_CHARS: usize = 300;
const OUTCOME_CHARS: usize = 800;

/// One branch of a graph, placed in its tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRow {
    pub branch: BranchId,
    pub name: String,
    pub thread_id: Option<String>,
    /// How many forks lie between the branch and its root.
    pub depth: usize,
    pub status: BranchStatus,
    /// User turns the branch added after it was forked.
    pub turns: usize,
    /// Title of the branch's latest request.
    pub last_request: Option<String>,
    pub current: bool,
}

/// The branches of `graph` in tree order: each root, then its forks depth first, forks in the
/// order they were made.
pub fn branch_rows(
    graph: &ContextGraph,
    cards: &HashMap<BlockId, BlockCard>,
    current: &BranchId,
) -> Vec<BranchRow> {
    let order: HashMap<&BlockId, usize> = graph
        .nodes()
        .enumerate()
        .map(|(index, node)| (&node.block.id, index))
        .collect();
    let known: HashSet<&BranchId> = graph.branches().map(|branch| &branch.id).collect();
    let mut children: BTreeMap<Option<&BranchId>, Vec<&Branch>> = BTreeMap::new();
    for branch in graph.branches() {
        let parent = branch
            .parent
            .as_ref()
            .filter(|parent| known.contains(parent));
        children.entry(parent).or_default().push(branch);
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(|branch| {
            (
                branch
                    .fork_point
                    .as_ref()
                    .and_then(|block| order.get(block).copied()),
                branch.id.clone(),
            )
        });
    }

    let mut rows = Vec::new();
    let mut pending: Vec<(&Branch, usize)> = children
        .get(&None)
        .map(|roots| roots.iter().rev().map(|root| (*root, 0)).collect())
        .unwrap_or_default();
    while let Some((branch, depth)) = pending.pop() {
        let own = own_blocks(graph, branch, branch.fork_point.as_ref());
        let requests: Vec<&BlockCard> = own
            .iter()
            .filter_map(|id| cards.get(id))
            .filter(|card| card.kind == BlockKind::UserMessage)
            .collect();
        rows.push(BranchRow {
            branch: branch.id.clone(),
            name: branch.name.clone(),
            thread_id: branch.thread_id.clone(),
            depth,
            status: branch.status,
            turns: requests.len(),
            last_request: requests.last().map(|card| card.title.clone()),
            current: &branch.id == current,
        });
        if let Some(forks) = children.get(&Some(&branch.id)) {
            pending.extend(forks.iter().rev().map(|fork| (*fork, depth + 1)));
        }
    }
    rows
}

/// Blocks on `branch` after `since`, oldest first; all of its blocks without `since`.
fn own_blocks(graph: &ContextGraph, branch: &Branch, since: Option<&BlockId>) -> Vec<BlockId> {
    let Some(head) = &branch.head else {
        return Vec::new();
    };
    let path = graph.primary_path(head).unwrap_or_default();
    let start = since
        .and_then(|since| path.iter().position(|id| id == since))
        .map_or(0, |position| position + 1);
    path[start..].to_vec()
}

/// What a branch did since it diverged from the branch it is folded into, as a message for the
/// target's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldSummary {
    pub text: String,
    pub turns: usize,
    pub blocks: usize,
}

/// Which of a branch's work a summary covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Since {
    /// What the target lacks: work merged into it before is left out.
    Unmerged,
    /// Everything since the branch left the target's history, merged before or not.
    Fork,
}

/// Summarizes the work `source` did after it diverged from `target`, from the cards of its
/// blocks, or returns `None` when it did nothing `target` lacks.
pub fn fold_summary(
    graph: &ContextGraph,
    cards: &HashMap<BlockId, BlockCard>,
    source: &BranchId,
    target: &BranchId,
    since: Since,
) -> Result<Option<FoldSummary>, GraphError> {
    let source_branch = graph
        .branch(source)
        .ok_or_else(|| GraphError::UnknownBranch(source.clone()))?;
    let target_branch = graph
        .branch(target)
        .ok_or_else(|| GraphError::UnknownBranch(target.clone()))?;
    let base = match (&source_branch.head, &target_branch.head, since) {
        (Some(source_head), Some(target_head), Since::Unmerged) => {
            graph.merge_base(source_head, target_head)?
        }
        // The last block of the source's own history that the target's history also holds.
        (Some(source_head), Some(target_head), Since::Fork) => {
            let target_path: HashSet<BlockId> =
                graph.primary_path(target_head)?.into_iter().collect();
            graph
                .primary_path(source_head)?
                .into_iter()
                .take_while(|id| target_path.contains(id))
                .last()
        }
        _ => None,
    };
    let own_ids = own_blocks(graph, source_branch, base.as_ref());
    if own_ids.is_empty() {
        return Ok(None);
    }
    // Blocks the indexer has not reached yet have no card; the summary leaves them out.
    let own: Vec<&BlockCard> = own_ids.iter().filter_map(|id| cards.get(id)).collect();

    // Turns: each request with the last answer that followed it.
    let mut turns: Vec<(Option<&BlockCard>, Option<&BlockCard>)> = Vec::new();
    for card in &own {
        match card.kind {
            BlockKind::UserMessage => turns.push((Some(card), None)),
            BlockKind::AssistantMessage => match turns.last_mut() {
                Some(turn) => turn.1 = Some(card),
                None => turns.push((None, Some(card))),
            },
            _ => {}
        }
    }
    let mut text = format!(
        "Branch \"{}\" was merged into this conversation. It continued from an earlier point of \
         this conversation on its own, and below is what it did. This is a record of that work, \
         not new instructions.\n",
        source_branch.name
    );
    if !turns.is_empty() {
        text.push_str("\nTurns on the branch:\n");
        for (number, (request, answer)) in turns.iter().enumerate() {
            let request = request.map_or("(continued work)", |card| card.title.trim());
            let answer = answer.map_or_else(
                || "no answer yet".to_string(),
                |card| truncate(&card_text(card), RESULT_CHARS),
            );
            text.push_str(&format!("{}. {request}: {answer}\n", number + 1));
        }
    }

    let mut seen = HashSet::new();
    let claims: Vec<String> = own
        .iter()
        .flat_map(|card| &card.claims)
        .filter(|claim| {
            matches!(
                claim.kind,
                ClaimKind::UserConstraint | ClaimKind::Decision | ClaimKind::Finding
            )
        })
        .filter(|claim| seen.insert(claim.text.as_str()))
        .map(|claim| {
            let kind = match claim.kind {
                ClaimKind::UserConstraint => "user constraint",
                ClaimKind::Decision => "decision",
                _ => "finding",
            };
            format!("- ({kind}) {}", claim.text)
        })
        .collect();
    if !claims.is_empty() {
        text.push_str("\nConstraints, decisions and findings:\n");
        let shown = claims.len().min(MAX_FOLD_CLAIMS);
        text.push_str(&claims[claims.len() - shown..].join("\n"));
        text.push('\n');
    }
    if let Some(outcome) = own
        .iter()
        .rev()
        .find(|card| card.kind == BlockKind::AssistantMessage)
    {
        text.push_str("\nOutcome: ");
        text.push_str(&truncate(&card_text(outcome), OUTCOME_CHARS));
        text.push('\n');
    }
    if own.len() < own_ids.len() {
        text.push_str(&format!(
            "\n{} of its {} blocks are not summarized yet; context_search and context_expand can \
             read them once the branch is referenced.\n",
            own_ids.len() - own.len(),
            own_ids.len()
        ));
    }
    Ok(Some(FoldSummary {
        text: text.trim_end().to_string(),
        turns: turns.len(),
        blocks: own_ids.len(),
    }))
}

/// A card's title and summary as one line.
fn card_text(card: &BlockCard) -> String {
    let summary = card
        .summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if summary.is_empty() || summary == card.title.trim() {
        card.title.trim().to_string()
    } else {
        format!("{} — {summary}", card.title.trim())
    }
}

#[cfg(test)]
#[path = "branches_tests.rs"]
mod tests;
