-- What users said about a thread's branch: what it is for, and whether they archived it. Kept by
-- thread, like names, so settings given before a thread first records still apply.
CREATE TABLE cg_thread_settings (
    thread_id TEXT PRIMARY KEY NOT NULL,
    purpose TEXT,
    archived INTEGER NOT NULL DEFAULT 0
);

-- What a merge was for, and the code merged along with the branch's context (JSON-encoded
-- CodeMerge).
ALTER TABLE cg_merges ADD COLUMN goal TEXT;
ALTER TABLE cg_merges ADD COLUMN code TEXT;

-- Signals about recall: blocks the model read with context_expand, whether the view held them,
-- and recalls a user asked for.
CREATE TABLE cg_recall_feedback (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    graph_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    block_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_cg_recall_feedback ON cg_recall_feedback(graph_id, branch_id);
