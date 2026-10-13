use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::*;

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

fn thread(id: &str, forked_from: Option<&str>) -> SavedThread {
    SavedThread {
        thread_id: id.to_string(),
        forked_from: forked_from.map(str::to_string),
        name: None,
    }
}

fn saved(threads: &[(&str, Vec<RolloutItem>)]) -> HashMap<String, Vec<RolloutItem>> {
    threads
        .iter()
        .map(|(id, items)| (id.to_string(), items.clone()))
        .collect()
}

async fn store(dir: &TempDir) -> LocalContextGraphStore {
    let home = AbsolutePathBuf::from_absolute_path(dir.path())
        .unwrap_or_else(|err| panic!("absolute temp dir: {err}"));
    LocalContextGraphStore::open(&SqliteConfig::new_for_testing(home))
        .await
        .unwrap_or_else(|err| panic!("open store: {err}"))
}

fn shared() -> Vec<RolloutItem> {
    vec![
        message("msg_u1", "user", "add retries to the client"),
        message("msg_a1", "assistant", "added retries"),
    ]
}

#[tokio::test]
async fn forks_join_their_parents_graph_in_any_order() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store(&dir).await;
    let mut fork_items = shared();
    fork_items.push(message("msg_f1", "user", "try jitter instead"));
    let mut fork = thread("t-fork", Some("t-main"));
    fork.name = Some("jitter".to_string());
    let mut main_items = shared();
    main_items.push(message("msg_u2", "user", "now write the docs"));
    let items = saved(&[("t-fork", fork_items), ("t-main", main_items)]);
    // The fork comes first; the import still records its parent first.
    let threads = vec![fork, thread("t-main", None)];

    let report = store
        .import_threads(&items, &threads)
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(
        report,
        ImportReport {
            recorded: 2,
            ..ImportReport::default()
        }
    );

    let tree = store
        .branch_tree("t-fork")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("no tree"));
    let rows: Vec<(String, usize, bool)> = tree
        .rows()
        .into_iter()
        .map(|row| (row.name, row.depth, row.current))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("main".to_string(), 0, false),
            ("jitter".to_string(), 1, true),
        ]
    );
    let fork_branch = tree
        .graph
        .branches()
        .find(|branch| branch.thread_id.as_deref() == Some("t-fork"))
        .cloned()
        .unwrap_or_else(|| panic!("no fork branch"));
    assert_eq!(
        fork_branch.fork_point,
        Some(BlockId::for_item_key("msg_a1"))
    );
    // Cards were extracted, so the tree knows each branch's latest request.
    assert!(tree.cards.contains_key(&BlockId::for_item_key("msg_f1")));

    // A second import finds both recorded.
    let again = store
        .import_threads(&items, &threads)
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(again.already_recorded, 2);
    assert_eq!(again.recorded, 0);
}

#[tokio::test]
async fn merges_of_an_earlier_merge_tool_join_the_graphs() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store(&dir).await;
    let bundle = serde_json::json!({
        "format": "codex-context-merge-v1",
        "baseThreadId": "t-base",
        "sourceThreadIds": ["t-base", "t-other"],
        "branches": [],
    });
    let mut merged_items = vec![message("msg_b1", "user", "plan the release")];
    merged_items.push(message(
        "msg_merge",
        "user",
        &format!("Codex context merge (historical branch data):\n{bundle}"),
    ));
    let items = saved(&[
        (
            "t-base",
            vec![message("msg_b1", "user", "plan the release")],
        ),
        (
            "t-other",
            vec![message("msg_o1", "user", "check the changelog")],
        ),
        ("t-merged", merged_items),
    ]);
    let threads = vec![
        thread("t-base", None),
        thread("t-other", None),
        thread("t-merged", Some("t-base")),
    ];

    let report = store
        .import_threads(&items, &threads)
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(report.recorded, 3);
    assert_eq!(report.merges, 1);

    let (merged_graph, merged) = store
        .branch_of_thread("t-merged")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("merged thread not recorded"));
    let (other_graph, other) = store
        .branch_of_thread("t-other")
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("other thread not recorded"));
    assert_eq!(other_graph, merged_graph);
    let merges = store
        .load_merges(&merged_graph)
        .await
        .unwrap_or_else(|err| panic!("merges: {err}"));
    assert_eq!(merges.len(), 1);
    assert_eq!(merges[0].target, merged.id);
    assert_eq!(merges[0].source, other.id);
    assert_eq!(merges[0].mode, MergeMode::Inline);
    assert_eq!(
        merges[0].merge_block,
        Some(BlockId::for_item_key("msg_merge"))
    );
    assert!(store.load_graph("t-other").await.ok().flatten().is_none());

    // Importing again records no second merge.
    let again = store
        .import_threads(&items, &threads)
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(again.merges, 0);
}

#[tokio::test]
async fn threads_without_items_are_reported() {
    let dir = TempDir::new().unwrap_or_else(|err| panic!("temp dir: {err}"));
    let store = store(&dir).await;
    let report = store
        .import_threads(&saved(&[]), &[thread("t-gone", None)])
        .await
        .unwrap_or_else(|err| panic!("import: {err}"));
    assert_eq!(report.recorded, 0);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "t-gone");
}

#[test]
fn blocks_are_ordered_after_their_parents() {
    let blocks: Vec<String> = ["a", "m", "x", "y"].map(String::from).to_vec();
    // `m` merges `y`, which was stored after it.
    let edges = [("m", "a"), ("m", "y"), ("y", "x")].map(|(c, p)| (c.to_string(), p.to_string()));
    assert_eq!(parents_first(&blocks, &edges), vec!["a", "x", "y", "m"]);
}

#[test]
fn source_fields_are_collected_once_each() {
    let value = serde_json::json!({
        "baseThreadId": "a",
        "sourceThreadIds": ["a", "b"],
        "branches": [{ "records": [{ "sourceThreadId": "c" }, { "sourceThreadId": "b" }] }],
    });
    let mut sources = Vec::new();
    collect_sources(&value, &mut sources);
    sources.sort();
    assert_eq!(sources, vec!["a", "b", "c"]);
}
