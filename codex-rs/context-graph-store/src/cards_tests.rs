use codex_context_graph::BlockKind;
use codex_context_graph::card::CARD_VERSION;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

fn card(indexer: &str, version: u32, title: &str) -> BlockCard {
    BlockCard {
        block_id: BlockId::from_stored("blk_1"),
        kind: BlockKind::UserMessage,
        turn_id: None,
        card_version: version,
        indexer: indexer.to_string(),
        title: title.to_string(),
        summary: String::new(),
        entities: Default::default(),
        tools: Vec::new(),
        outcome: None,
        exit_code: None,
        claims: Vec::new(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        file_stamps: Vec::new(),
        tokens: 1,
    }
}

async fn open_store(dir: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"))
}

async fn stored_title(store: &LocalContextGraphStore) -> Option<String> {
    store
        .load_cards("graph-1")
        .await
        .ok()
        .and_then(|cards| cards.into_iter().next())
        .map(|card| card.title)
}

#[tokio::test]
async fn extracted_cards_never_replace_model_cards_of_the_same_version() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    let write = |card: BlockCard| {
        let store = store.clone();
        async move {
            store
                .upsert_card("graph-1", &card)
                .await
                .unwrap_or_else(|err| panic!("upsert: {err}"));
        }
    };

    write(card(EXTRACTED_INDEXER, CARD_VERSION, "extracted")).await;
    write(card("model-a", CARD_VERSION, "model")).await;
    write(card(EXTRACTED_INDEXER, CARD_VERSION, "extracted again")).await;
    assert_eq!(stored_title(&store).await.as_deref(), Some("model"));
    assert!(
        store
            .has_model_card("graph-1", &BlockId::from_stored("blk_1"), CARD_VERSION)
            .await
            .unwrap_or_default()
    );

    // A newer model card and a newer card version both replace it.
    write(card("model-b", CARD_VERSION, "model b")).await;
    assert_eq!(stored_title(&store).await.as_deref(), Some("model b"));
    write(card(EXTRACTED_INDEXER, CARD_VERSION + 1, "rebuilt")).await;
    assert_eq!(stored_title(&store).await.as_deref(), Some("rebuilt"));
    assert!(
        !store
            .has_model_card("graph-1", &BlockId::from_stored("blk_1"), CARD_VERSION + 1)
            .await
            .unwrap_or(true)
    );
}

#[tokio::test]
async fn index_usage_sums_requests() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = open_store(&dir).await;
    for (latency_ms, error) in [(100, None), (300, Some("timeout".to_string()))] {
        store
            .record_index_request(
                "graph-1",
                &IndexRequest {
                    indexer: "model-a".to_string(),
                    blocks: 4,
                    input_tokens: 1_000,
                    cached_input_tokens: 600,
                    output_tokens: 50,
                    latency_ms,
                    error,
                },
            )
            .await
            .unwrap_or_else(|err| panic!("record: {err}"));
    }
    assert_eq!(
        store.index_usage("graph-1").await.ok(),
        Some(IndexUsage {
            requests: 2,
            failed_requests: 1,
            blocks: 8,
            input_tokens: 2_000,
            cached_input_tokens: 1_200,
            output_tokens: 100,
            latency_ms_total: 400,
            latency_ms_max: 300,
        })
    );
}
