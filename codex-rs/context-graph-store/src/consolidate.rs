//! Consolidation: while a session is idle, the shadow agent sums up runs of settled turns as
//! episode cards and the branch as a branch card, with its model or from the turns themselves.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

use codex_context_graph::BlockId;
use codex_context_graph::BranchId;
use codex_context_graph::card::BlockCard;
use codex_context_graph::episode::BranchCard;
use codex_context_graph::episode::EpisodeCard;
use codex_context_graph::episode::branch_prompt;
use codex_context_graph::episode::episode_prompt;
use codex_context_graph::episode::extracted_branch_card;
use codex_context_graph::episode::extracted_episodes;
use codex_context_graph::episode::parse_branch_card;
use codex_context_graph::episode::parse_episodes;
use codex_context_graph::episode::turn_lines;
use codex_context_graph::episode::unsettled_turns;
use codex_context_graph::prompt::ModelPrompt;
use tracing::warn;

use crate::IndexRequest;
use crate::LocalContextGraphStore;
use crate::ShadowModel;

/// New turns after which a branch card is written again.
const BRANCH_CARD_TURNS: usize = 3;

/// What a consolidation wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Consolidation {
    pub episodes: usize,
    pub branch_card: bool,
    /// Requests made to the model.
    pub requests: u64,
}

impl LocalContextGraphStore {
    /// Sums up the settled turns of `branch` that no episode covers yet as episode cards, and
    /// writes its branch card when it has none or has grown since. Without `model`, or when the
    /// model's answer is unusable, the cards are made from the turns' block cards.
    pub async fn consolidate_branch(
        &self,
        graph_id: &str,
        branch: &BranchId,
        model: Option<&dyn ShadowModel>,
    ) -> anyhow::Result<Consolidation> {
        let mut result = Consolidation::default();
        let Some(graph) = self.load_graph(graph_id).await? else {
            return Ok(result);
        };
        let Some(stored) = graph.branch(branch) else {
            return Ok(result);
        };
        let Some(head) = &stored.head else {
            return Ok(result);
        };
        let path = graph.primary_path(head)?;
        let cards: HashMap<BlockId, BlockCard> = self
            .load_cards(graph_id)
            .await?
            .into_iter()
            .map(|card| (card.block_id.clone(), card))
            .collect();
        let turns = turn_lines(path.iter().filter_map(|id| cards.get(id)));
        let on_path: HashSet<&BlockId> = path.iter().collect();
        let mut episodes: Vec<EpisodeCard> = self
            .load_episode_cards(graph_id)
            .await?
            .into_iter()
            .filter(|episode| on_path.contains(&episode.first_block))
            .collect();

        let unsettled = unsettled_turns(&turns, &episodes);
        if !unsettled.is_empty() {
            let mut made = None;
            if let Some(model) = model {
                result.requests += 1;
                made = self
                    .ask(
                        graph_id,
                        model,
                        unsettled.len(),
                        &episode_prompt(&episodes, unsettled),
                    )
                    .await
                    .and_then(|text| {
                        parse_episodes(&text, unsettled, &model.name())
                            .map_err(|err| warn!("context graph episodes unusable: {err:#}"))
                            .ok()
                    });
            }
            let made = made.unwrap_or_else(|| extracted_episodes(unsettled));
            self.save_episode_cards(graph_id, &made).await?;
            result.episodes = made.len();
            episodes.extend(made);
        }

        // A fork's card describes what it did after it left its parent.
        let own_start = stored
            .fork_point
            .as_ref()
            .and_then(|fork_point| path.iter().position(|id| id == fork_point))
            .map_or(0, |index| index + 1);
        let own_turns = turn_lines(path[own_start..].iter().filter_map(|id| cards.get(id)));
        if own_turns.is_empty() {
            return Ok(result);
        }
        let due = match self.load_branch_cards(graph_id).await?.get(branch) {
            None => true,
            Some(card) => result.episodes > 0 || turns.len() >= card.turns + BRANCH_CARD_TURNS,
        };
        if !due {
            return Ok(result);
        }
        let mut card: Option<BranchCard> = None;
        if let Some(model) = model {
            result.requests += 1;
            let prompt = branch_prompt(&stored.name, &episodes, &own_turns);
            card = self
                .ask(graph_id, model, own_turns.len(), &prompt)
                .await
                .and_then(|text| {
                    parse_branch_card(&text, &model.name(), turns.len())
                        .map_err(|err| warn!("context graph branch card unusable: {err:#}"))
                        .ok()
                });
        }
        if let Some(card) = card.or_else(|| extracted_branch_card(&own_turns, turns.len())) {
            self.save_branch_card(graph_id, branch, &card).await?;
            result.branch_card = true;
        }
        Ok(result)
    }

    /// Asks `model` and records the request; `None` when it failed.
    async fn ask(
        &self,
        graph_id: &str,
        model: &dyn ShadowModel,
        turns: usize,
        prompt: &ModelPrompt,
    ) -> Option<String> {
        let started = Instant::now();
        let completion = model.complete(prompt).await;
        let mut request = IndexRequest {
            indexer: model.name(),
            blocks: u32::try_from(turns).unwrap_or(u32::MAX),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ..IndexRequest::default()
        };
        let text = match completion {
            Ok(completion) => {
                request.input_tokens = completion.input_tokens;
                request.cached_input_tokens = completion.cached_input_tokens;
                request.output_tokens = completion.output_tokens;
                Some(completion.text)
            }
            Err(err) => {
                request.error = Some(format!("{err:#}"));
                None
            }
        };
        if let Err(err) = self.record_index_request(graph_id, &request).await {
            warn!("context graph request record failed: {err:#}");
        }
        text
    }
}

#[cfg(test)]
#[path = "consolidate_tests.rs"]
mod tests;
