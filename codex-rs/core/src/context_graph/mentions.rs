//! Branches the user names with `@` in a request.
//!
//! In a session that recalls, a mentioned branch becomes a reference: recall chooses from its
//! history from then on. Otherwise its work is folded into the history as a summary.

use std::collections::HashSet;

use codex_context_graph_store::MergeMode;
use tracing::warn;

use super::ContextGraphService;
use super::PreparedMerge;
use crate::session::session::Session;

/// Merges the branches `question` mentions and returns the merges, whose notes go into the
/// history before the turn samples and which are recorded once they are there. Mentions that
/// name no branch of the conversation are ordinary text.
pub(crate) async fn resolve_branch_mentions(sess: &Session, question: &str) -> Vec<PreparedMerge> {
    let Some(service) = sess.services.context_graph.as_ref() else {
        return Vec::new();
    };
    let names = mentioned_names(question);
    if names.is_empty() {
        return Vec::new();
    }
    match service.merge_mentions(&names).await {
        Ok(merges) => merges,
        Err(err) => {
            warn!("branch mentions could not be resolved: {err:#}");
            Vec::new()
        }
    }
}

impl ContextGraphService {
    async fn merge_mentions(&self, names: &[String]) -> anyhow::Result<Vec<PreparedMerge>> {
        let store = self.live.store().await?;
        let Some(tree) = store.branch_tree(&self.live.membership().thread_id).await? else {
            return Ok(Vec::new());
        };
        let mode = if self.recalls() {
            MergeMode::Reference
        } else {
            MergeMode::Fold
        };
        let referenced: HashSet<_> = tree
            .references()
            .into_iter()
            .map(|merge| merge.source.clone())
            .collect();
        let mut seen = HashSet::new();
        let mut merges = Vec::new();
        for name in names {
            let Some(branch) = tree.find(name) else {
                continue;
            };
            if branch.current
                || !seen.insert(branch.branch.clone())
                || (mode == MergeMode::Reference && referenced.contains(&branch.branch))
            {
                continue;
            }
            match self.prepare_merge(name, mode).await {
                Ok(merge) => merges.push(merge),
                Err(err) => warn!("could not merge branch {name}: {err:#}"),
            }
        }
        Ok(merges)
    }
}

/// Names written as `@name`, where `@` starts a word, in order of appearance.
fn mentioned_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut previous = ' ';
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        // Text in scripts without spaces, such as Chinese, may run right up to the `@`.
        let starts_word = !(previous.is_ascii_alphanumeric() || previous == '_' || previous == '.');
        previous = ch;
        if ch != '@' || !starts_word {
            continue;
        }
        let mut end = start + 1;
        while let Some((index, next)) = chars.peek().copied() {
            if next.is_alphanumeric() || matches!(next, '-' | '_' | '.') {
                end = index + next.len_utf8();
                previous = next;
                chars.next();
            } else {
                break;
            }
        }
        let name = text[start + 1..end].trim_end_matches('.');
        if !name.is_empty() && !names.iter().any(|known| known == name) {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
#[path = "mentions_tests.rs"]
mod tests;
