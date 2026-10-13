use std::sync::Mutex;

use codex_context_graph::Branch;
use codex_context_graph::ContextGraph;
use codex_context_graph::DefaultClassifier;
use codex_context_graph::GraphRecorder;
use codex_context_graph::card::ClaimKind;
use codex_context_graph::card::EXTRACTED_INDEXER;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::IndexUsage;

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

fn exec(id: &str, output_id: &str, command: &str, output: &str) -> Vec<RolloutItem> {
    vec![
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(ResponseItem::FunctionCall {
            encrypted_content: None,
            status: None,
            id: Some(ResponseItemId::from_server(id.to_string())),
            name: "exec_command".to_string(),
            namespace: None,
            arguments: json!({ "cmd": command, "workdir": "/repo" }).to_string(),
            encrypted_function_args: None,
            call_id: format!("call_{id}"),
            internal_chat_message_metadata_passthrough: None,
        })),
        RolloutItem::ResponseItem(ResponseItemEnvelope::new(
            ResponseItem::FunctionCallOutput {
                id: Some(ResponseItemId::from_server(output_id.to_string())),
                call_id: Some(format!("call_{id}")),
                name: None,
                namespace: None,
                output: FunctionCallOutputPayload::from_text(output.to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        )),
    ]
}

fn turn_complete(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: turn_id.to_string(),
        last_agent_message: None,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

/// One turn: a request, a test run and an answer.
fn turn() -> Vec<RolloutItem> {
    let mut items = vec![message(
        "msg_u1",
        "user",
        "Fix the flaky test. Do not touch the public API. key sk-abcdefghijklmnopqrstuvwxyz123456",
    )];
    items.extend(exec(
        "fc_1",
        "fco_1",
        "cargo test retry",
        "Process exited with code 0\n3 passed",
    ));
    items.push(message("msg_a1", "assistant", "The retry test passes now."));
    items.push(turn_complete("turn_1"));
    items
}

fn events(items: &[RolloutItem]) -> Vec<RecorderEvent> {
    let branch = BranchId::new("br_main");
    let mut graph = ContextGraph::new();
    graph
        .add_branch(Branch::new(branch.clone(), "main"))
        .unwrap_or_else(|err| panic!("add branch: {err}"));
    let mut recorder = GraphRecorder::new(branch, DefaultClassifier).with_events();
    for item in items {
        recorder
            .observe(&mut graph, item)
            .unwrap_or_else(|err| panic!("observe: {err}"));
    }
    recorder.take_events()
}

/// Answers every batch with titles derived from the block IDs, or fails.
struct FakeModel {
    fail: bool,
    /// A block the first answer leaves out.
    skip_once: Mutex<Option<String>>,
    batches: Mutex<Vec<Vec<String>>>,
}

impl FakeModel {
    fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            fail,
            skip_once: Mutex::new(None),
            batches: Mutex::new(Vec::new()),
        })
    }

    fn skipping_once(block_id: &str) -> Arc<Self> {
        Arc::new(Self {
            fail: false,
            skip_once: Mutex::new(Some(block_id.to_string())),
            batches: Mutex::new(Vec::new()),
        })
    }

    fn batches(&self) -> Vec<Vec<String>> {
        self.batches
            .lock()
            .map(|batches| batches.clone())
            .unwrap_or_default()
    }
}

impl CardModel for FakeModel {
    fn name(&self) -> &str {
        "fake-model"
    }

    fn complete<'a>(
        &'a self,
        prompt: &'a CardPrompt,
    ) -> BoxFuture<'a, anyhow::Result<CardCompletion>> {
        Box::pin(async move {
            let ids: Vec<String> = prompt.batch.iter().map(ToString::to_string).collect();
            if let Ok(mut batches) = self.batches.lock() {
                batches.push(ids.clone());
            }
            if self.fail {
                anyhow::bail!("model unavailable");
            }
            let skip = self.skip_once.lock().ok().and_then(|mut skip| skip.take());
            let cards: Vec<_> = ids
                .iter()
                .filter(|id| skip.as_ref() != Some(*id))
                .map(|id| {
                    let claims = if id == "blk_msg_u1" {
                        json!([{ "kind": "user_constraint", "text": "do not touch the public API" }])
                    } else {
                        json!([])
                    };
                    json!({
                        "block_id": id,
                        "title": format!("model title for {id}"),
                        "summary": "model summary",
                        "claims": claims,
                        "depends_on": [],
                        "supersedes": [],
                    })
                })
                .collect();
            Ok(CardCompletion {
                text: json!({ "cards": cards }).to_string(),
                input_tokens: 100,
                cached_input_tokens: 40,
                output_tokens: 10,
            })
        })
    }
}

async fn open_store(dir: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"))
}

async fn index(
    store: &LocalContextGraphStore,
    model: Option<Arc<dyn CardModel>>,
    config: IndexerConfig,
    events: Vec<RecorderEvent>,
) {
    let mut indexer = ShadowIndexer::new(
        store.clone(),
        "graph-1",
        BranchId::new("br_main"),
        model,
        config,
    );
    for event in events {
        indexer
            .observe(event)
            .await
            .unwrap_or_else(|err| panic!("observe: {err}"));
    }
    indexer.finish().await;
}

fn small_batches() -> IndexerConfig {
    IndexerConfig {
        batch_blocks: 2,
        ..IndexerConfig::default()
    }
}

async fn cards(store: &LocalContextGraphStore) -> Vec<BlockCard> {
    store
        .load_cards("graph-1")
        .await
        .unwrap_or_else(|err| panic!("load cards: {err}"))
}

#[tokio::test]
async fn without_a_model_blocks_and_turns_get_extracted_cards() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    index(&store, None, IndexerConfig::default(), events(&turn())).await;

    let cards = cards(&store).await;
    let titles: Vec<&str> = cards.iter().map(|card| card.title.as_str()).collect();
    assert_eq!(
        titles,
        vec![
            "Fix the flaky test. Do not touch the public API. key [REDACTED_SECRET]",
            "$ cargo test retry",
            "The retry test passes now.",
        ]
    );
    assert!(cards.iter().all(|card| card.indexer == EXTRACTED_INDEXER));

    let turns = store
        .load_turn_cards("graph-1")
        .await
        .unwrap_or_else(|err| panic!("load turns: {err}"));
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].turn_id, "turn_1");
    assert_eq!(
        store.index_usage("graph-1").await.ok(),
        Some(IndexUsage::default())
    );
}

#[tokio::test]
async fn model_cards_replace_extracted_text_in_batches() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    let model = FakeModel::new(false);
    index(
        &store,
        Some(model.clone()),
        small_batches(),
        events(&turn()),
    )
    .await;

    assert_eq!(
        model.batches(),
        vec![
            vec!["blk_msg_u1".to_string(), "blk_fc_1".to_string()],
            vec!["blk_msg_a1".to_string()],
        ]
    );
    let cards = cards(&store).await;
    assert!(cards.iter().all(|card| card.indexer == "fake-model"));
    assert_eq!(cards[0].title, "model title for blk_msg_u1");
    assert_eq!(cards[0].claims[0].kind, ClaimKind::UserConstraint);
    // Extracted facts survive the model's rewrite.
    assert_eq!(cards[1].exit_code, Some(0));
    assert_eq!(
        store.index_usage("graph-1").await.ok().map(|usage| (
            usage.requests,
            usage.blocks,
            usage.input_tokens,
            usage.cached_input_tokens,
            usage.output_tokens
        )),
        Some((2, 3, 200, 80, 20))
    );
}

#[tokio::test]
async fn replays_keep_model_cards_and_skip_indexed_blocks() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    index(
        &store,
        Some(FakeModel::new(false)),
        small_batches(),
        events(&turn()),
    )
    .await;

    // A resumed session replays the same history, with and without a model.
    index(&store, None, small_batches(), events(&turn())).await;
    let again = FakeModel::new(false);
    index(
        &store,
        Some(again.clone()),
        small_batches(),
        events(&turn()),
    )
    .await;

    assert_eq!(again.batches(), Vec::<Vec<String>>::new());
    assert!(
        cards(&store)
            .await
            .iter()
            .all(|card| card.indexer == "fake-model")
    );
}

#[tokio::test]
async fn failed_requests_keep_extracted_cards_and_are_counted() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    index(
        &store,
        Some(FakeModel::new(true)),
        small_batches(),
        events(&turn()),
    )
    .await;

    assert!(
        cards(&store)
            .await
            .iter()
            .all(|card| card.indexer == EXTRACTED_INDEXER)
    );
    let usage = store
        .index_usage("graph-1")
        .await
        .unwrap_or_else(|err| panic!("usage: {err}"));
    // Each failed batch is retried once.
    assert_eq!((usage.requests, usage.failed_requests), (4, 4));
}

#[tokio::test]
async fn blocks_an_answer_misses_are_retried_once() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    let model = FakeModel::skipping_once("blk_msg_u1");
    index(
        &store,
        Some(model.clone()),
        small_batches(),
        events(&turn()),
    )
    .await;

    let mut batches = model.batches();
    batches.sort();
    assert_eq!(
        batches,
        vec![
            vec!["blk_msg_a1".to_string()],
            vec!["blk_msg_u1".to_string()],
            vec!["blk_msg_u1".to_string(), "blk_fc_1".to_string()],
        ]
    );
    assert!(
        cards(&store)
            .await
            .iter()
            .all(|card| card.indexer == "fake-model")
    );
    let usage = store
        .index_usage("graph-1")
        .await
        .unwrap_or_else(|err| panic!("usage: {err}"));
    assert_eq!((usage.requests, usage.failed_requests), (3, 1));
}

#[tokio::test]
async fn the_request_budget_is_respected() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    let model = FakeModel::new(false);
    let config = IndexerConfig {
        max_requests: Some(1),
        ..small_batches()
    };
    index(&store, Some(model.clone()), config, events(&turn())).await;
    assert_eq!(model.batches().len(), 1);
}

#[tokio::test]
async fn running_closes_batches_after_the_wait() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    let model = FakeModel::new(false);
    let config = IndexerConfig {
        batch_blocks: 8,
        batch_wait: Duration::from_millis(20),
        ..IndexerConfig::default()
    };
    let indexer = ShadowIndexer::new(
        store.clone(),
        "graph-1",
        BranchId::new("br_main"),
        Some(model.clone()),
        config,
    );
    let (sender, receiver) = mpsc::unbounded_channel();
    let task = tokio::spawn(indexer.run(receiver));
    // Only the user message, with no turn end: the wait alone must close the batch.
    for event in events(&turn()[..1]) {
        let _ = sender.send(event);
    }
    for _ in 0..200 {
        if !model.batches().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(model.batches(), vec![vec!["blk_msg_u1".to_string()]]);
    drop(sender);
    task.await
        .unwrap_or_else(|err| panic!("indexer task: {err}"));
}
