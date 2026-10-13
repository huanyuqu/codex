-- Context views that recall built, with what built them and what it cost: the record that makes
-- a recall reproducible and auditable, and data for evaluating selectors.

CREATE TABLE cg_views (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    graph_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    question TEXT NOT NULL,
    -- Who chose the blocks, such as a screening model or "bm25".
    selector TEXT NOT NULL,
    -- JSON-encoded ContextView.
    view TEXT NOT NULL,
    -- JSON-encoded RecallStats.
    stats TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_cg_views_graph ON cg_views(graph_id);
