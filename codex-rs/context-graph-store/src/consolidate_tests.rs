use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use codex_context_graph::prompt::ModelPrompt;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_rollout::RolloutItem;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;
use crate::ModelCompletion;
use crate::SavedThread;

fn message(id: &str, role: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: Some(ResponseItemId::from_server(id.to_string())),
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }))
}

/// A recorded thread `t-main` with `turns` turns of one request and one answer each.
async fn recorded(turns: usize) -> (TempDir, LocalContextGraphStore, String, BranchId) {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    let store = LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"));
    let items: Vec<RolloutItem> = (1..=turns)
        .flat_map(|turn| {
            [
                message(&format!("msg_u{turn}"), "user", &format!("request {turn}")),
                message(
                    &format!("msg_a{turn}"),
                    "assistant",
                    &format!("result {turn}"),
                ),
            ]
        })
        .collect();
    let saved = HashMap::from([("t-main".to_string(), items)]);
    store
        .import_threads(
            &saved,
            &[SavedThread {
                thread_id: "t-main".to_string(),
                forked_from: None,
                name: None,
            }],
        )
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    let (graph_id, branch) = store
        .branch_of_thread("t-main")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("not recorded"));
    (dir, store, graph_id, branch.id)
}

/// Answers episode and branch card prompts with fixed JSON.
struct ScriptedModel {
    episodes: &'static str,
    branch: &'static str,
    asked: Mutex<Vec<&'static str>>,
}

impl ShadowModel for ScriptedModel {
    fn name(&self) -> &str {
        "scripted"
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a ModelPrompt,
    ) -> BoxFuture<'a, anyhow::Result<ModelCompletion>> {
        Box::pin(async move {
            if let Ok(mut asked) = self.asked.lock() {
                asked.push(prompt.schema_name);
            }
            let text = match prompt.schema_name {
                "episodes" => self.episodes,
                _ => self.branch,
            };
            Ok(ModelCompletion {
                text: text.to_string(),
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 10,
            })
        })
    }
}

#[tokio::test]
async fn idle_consolidation_extracts_episodes_and_a_branch_card() {
    let (_dir, store, graph_id, branch) = recorded(10).await;
    let first = store
        .consolidate_branch(&graph_id, &branch, None)
        .await
        .unwrap_or_else(|err| panic!("consolidate: {err}"));
    assert_eq!(
        first,
        Consolidation {
            episodes: 1,
            branch_card: true,
            requests: 0,
        }
    );
    let episodes = store
        .load_episode_cards(&graph_id)
        .await
        .unwrap_or_else(|err| panic!("episodes: {err}"));
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].first_block, BlockId::for_item_key("msg_u1"));
    assert_eq!(episodes[0].last_block, BlockId::for_item_key("msg_u8"));
    let cards = store
        .load_branch_cards(&graph_id)
        .await
        .unwrap_or_else(|err| panic!("branch cards: {err}"));
    assert_eq!(
        cards
            .get(&branch)
            .map(|card| (card.title.as_str(), card.turns)),
        Some(("request 1", 10))
    );

    // Nothing new to sum up.
    let second = store
        .consolidate_branch(&graph_id, &branch, None)
        .await
        .unwrap_or_else(|err| panic!("consolidate: {err}"));
    assert_eq!(second, Consolidation::default());
}

#[tokio::test]
async fn a_model_groups_the_turns_and_describes_the_branch() {
    let (_dir, store, graph_id, branch) = recorded(8).await;
    let model = Arc::new(ScriptedModel {
        episodes: r#"{"episodes":[
            {"first_turn":1,"last_turn":2,"title":"setup","summary":"set up the client"},
            {"first_turn":3,"last_turn":6,"title":"retries","summary":"added retries"}]}"#,
        branch: r#"{"title":"HTTP client retries","summary":"Retries done."}"#,
        asked: Mutex::new(Vec::new()),
    });
    let result = store
        .consolidate_branch(&graph_id, &branch, Some(model.as_ref()))
        .await
        .unwrap_or_else(|err| panic!("consolidate: {err}"));
    assert_eq!(result.episodes, 2);
    assert_eq!(result.requests, 2);
    let titles: Vec<String> = store
        .load_episode_cards(&graph_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|episode| episode.title)
        .collect();
    assert_eq!(titles, vec!["setup", "retries"]);
    let card = store
        .load_branch_cards(&graph_id)
        .await
        .unwrap_or_default()
        .remove(&branch)
        .unwrap_or_else(|| panic!("no branch card"));
    assert_eq!(card.title, "HTTP client retries");
    assert_eq!(card.indexer, "scripted");
    let requests = store
        .load_index_requests(&graph_id)
        .await
        .unwrap_or_default();
    assert_eq!(requests.len(), 2);
}

#[tokio::test]
async fn unusable_model_answers_fall_back_to_extracted_cards() {
    let (_dir, store, graph_id, branch) = recorded(8).await;
    let model = ScriptedModel {
        episodes: r#"{"episodes":[{"first_turn":2,"last_turn":6,"title":"x","summary":""}]}"#,
        branch: "not json",
        asked: Mutex::new(Vec::new()),
    };
    let result = store
        .consolidate_branch(&graph_id, &branch, Some(&model))
        .await
        .unwrap_or_else(|err| panic!("consolidate: {err}"));
    assert_eq!(result.episodes, 1);
    assert!(result.branch_card);
    let card = store
        .load_branch_cards(&graph_id)
        .await
        .unwrap_or_default()
        .remove(&branch)
        .unwrap_or_else(|| panic!("no branch card"));
    assert_eq!(card.indexer, codex_context_graph::card::EXTRACTED_INDEXER);
}
