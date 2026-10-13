-- Episode cards sum up runs of turns, named by the user messages that start their first and
-- last turns; branch cards describe whole branches.
CREATE TABLE cg_episode_cards (
    graph_id TEXT NOT NULL,
    first_block TEXT NOT NULL,
    last_block TEXT NOT NULL,
    indexer TEXT NOT NULL,
    -- JSON-encoded EpisodeCard.
    card TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (graph_id, first_block)
);

CREATE TABLE cg_branch_cards (
    graph_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    indexer TEXT NOT NULL,
    -- JSON-encoded BranchCard.
    card TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (graph_id, branch_id)
);
