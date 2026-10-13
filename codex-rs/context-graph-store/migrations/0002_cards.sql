-- Index cards written by the shadow indexer. Derived data: rebuilt when card_version changes.

CREATE TABLE cg_cards (
    graph_id TEXT NOT NULL,
    block_id TEXT NOT NULL,
    card_version INTEGER NOT NULL,
    -- "extract" for cards built without a model, otherwise the indexing model.
    indexer TEXT NOT NULL,
    -- JSON-encoded BlockCard.
    card TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (graph_id, block_id)
);

CREATE TABLE cg_turn_cards (
    graph_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    indexer TEXT NOT NULL,
    -- JSON-encoded TurnCard.
    card TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (graph_id, turn_id)
);

-- One row per indexing model request, for cost and latency accounting.
CREATE TABLE cg_index_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    graph_id TEXT NOT NULL,
    indexer TEXT NOT NULL,
    blocks INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    error TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_cg_index_requests_graph ON cg_index_requests(graph_id);
