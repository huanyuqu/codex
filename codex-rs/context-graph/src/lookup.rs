//! Reading history that recall left out: searching cards and expanding blocks or turns, for the
//! agent's `context_search` and `context_expand` tools.

use crate::history::History;
use crate::render::kind_name;
use crate::search::rank_cards;
use crate::text::READ_BLOCK_CHARS;
use crate::text::READ_ITEM_CHARS;
use crate::text::render_block;
use crate::text::truncate;

const SUMMARY_CHARS: usize = 240;

/// The cards that best match `query`, one per line with the block ID to expand, best first.
pub fn search_history(history: &History<'_>, query: &str, limit: usize) -> String {
    let recallable: Vec<usize> = (0..history.len())
        .filter(|index| history.recallable(*index))
        .collect();
    let cards: Vec<_> = recallable
        .iter()
        .map(|index| history.card(*index))
        .collect();
    let hits: Vec<String> = rank_cards(&cards, query)
        .into_iter()
        .take(limit.max(1))
        .map(|(position, _)| {
            let index = recallable[position];
            let card = history.card(index);
            let mut line = format!(
                "{} ({}, {}): {}",
                card.block_id,
                place(history, index),
                kind_name(card.kind),
                card.title.trim()
            );
            let summary = card
                .summary
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !summary.is_empty() && summary != card.title.trim() {
                line.push_str(" — ");
                line.push_str(&truncate(&summary, SUMMARY_CHARS));
            }
            line
        })
        .collect();
    if hits.is_empty() {
        return format!("No earlier block matches \"{}\".", query.trim());
    }
    hits.join("\n")
}

/// The full text of the block named `block_id`.
pub fn expand_block(history: &History<'_>, block_id: &str) -> Option<String> {
    let index =
        (0..history.len()).find(|index| history.card(*index).block_id.as_str() == block_id)?;
    Some(block_text(history, index, READ_BLOCK_CHARS))
}

/// The text of every block of `turn`, sharing `max_chars` characters.
pub fn expand_turn(history: &History<'_>, turn: usize, max_chars: usize) -> Option<String> {
    let blocks: Vec<usize> = (0..history.len())
        .filter(|index| {
            history.turn(*index) == turn && history.recallable(*index) && history.is_own(*index)
        })
        .collect();
    if blocks.is_empty() {
        return None;
    }
    let per_block = (max_chars / blocks.len()).max(400);
    Some(
        blocks
            .into_iter()
            .map(|index| block_text(history, index, per_block))
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}

fn block_text(history: &History<'_>, index: usize, max_chars: usize) -> String {
    let card = history.card(index);
    format!(
        "## {} ({}, {}): {}\n{}",
        card.block_id,
        place(history, index),
        kind_name(card.kind),
        card.title.trim(),
        render_block(
            history.block(index).items,
            READ_ITEM_CHARS.min(max_chars),
            max_chars
        )
        .trim_end()
    )
}

/// Where a block sits: its turn, or the referenced branch it comes from.
fn place(history: &History<'_>, index: usize) -> String {
    match history.origin(index) {
        Some(branch) => format!("branch {branch}"),
        None => format!("turn {}", history.turn(index)),
    }
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
