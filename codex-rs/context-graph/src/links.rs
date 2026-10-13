//! Links between cards that follow from file and command facts alone.
//!
//! A write to a file supersedes every read of that file since the previous write, because what
//! those reads showed is no longer true. A command that runs after edits in the same turn, such
//! as a test run, depends on those edits. Re-reading a file does not supersede an earlier read,
//! since the two reads may cover different parts of it.

use std::collections::HashMap;

use crate::BlockId;
use crate::card::BlockCard;
use crate::card::Relation;

/// Edits a later command can depend on.
const MAX_EDIT_DEPENDENCIES: usize = 3;

/// Tracks reads and edits along one branch, in block order.
#[derive(Debug, Default, Clone)]
pub struct LinkTracker {
    reads_since_write: HashMap<String, Vec<BlockId>>,
    edits_since_command: Vec<BlockId>,
}

impl LinkTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the links `card` gets from earlier blocks, then records its own reads and edits.
    pub fn link(&mut self, card: &mut BlockCard) {
        self.link_with(card, true);
    }

    /// Records the reads and edits of a block that already has its card, such as history a fork
    /// inherited, so later blocks link to it.
    pub fn record(&mut self, card: &BlockCard) {
        let mut copy = card.clone();
        self.link_with(&mut copy, false);
    }

    /// Forgets edits that later commands could depend on; called when a turn ends.
    pub fn end_turn(&mut self) {
        self.edits_since_command.clear();
    }

    fn link_with(&mut self, card: &mut BlockCard, add_links: bool) {
        let entities = card.entities.clone();
        for file in &entities.files_written {
            if let Some(reads) = self.reads_since_write.remove(file)
                && add_links
            {
                card.link(Relation::Supersedes, reads);
            }
        }
        for file in &entities.files_read {
            self.reads_since_write
                .entry(file.clone())
                .or_default()
                .push(card.block_id.clone());
        }

        let ran_command = !entities.commands.is_empty() && card.exit_code.is_some();
        if ran_command && entities.files_written.is_empty() {
            if add_links {
                let edits = self
                    .edits_since_command
                    .iter()
                    .rev()
                    .take(MAX_EDIT_DEPENDENCIES)
                    .rev()
                    .cloned();
                card.link(Relation::DependsOn, edits);
            }
            self.edits_since_command.clear();
        } else if !entities.files_written.is_empty() {
            self.edits_since_command.push(card.block_id.clone());
        }
    }
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
