-- Merges between branches of a graph. The merged content enters the target thread's own history,
-- so these rows only record which branch was merged into which, and up to where.

CREATE TABLE cg_merges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    graph_id TEXT NOT NULL,
    target_branch_id TEXT NOT NULL,
    source_branch_id TEXT NOT NULL,
    -- Head of the source branch when it was merged.
    source_head TEXT,
    -- How it was merged, such as "fold".
    mode TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_cg_merges_graph ON cg_merges(graph_id);
