-- Derived index of context graphs. Every row can be rebuilt by replaying rollouts.

CREATE TABLE cg_branches (
    graph_id TEXT NOT NULL,
    branch_id TEXT NOT NULL,
    name TEXT NOT NULL,
    thread_id TEXT,
    parent_branch_id TEXT,
    fork_point TEXT,
    head TEXT,
    status TEXT NOT NULL,
    PRIMARY KEY (graph_id, branch_id)
);

CREATE TABLE cg_blocks (
    graph_id TEXT NOT NULL,
    block_id TEXT NOT NULL,
    -- Recording order within the graph; parents always have a lower seq.
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    owner_branch_id TEXT NOT NULL,
    turn_id TEXT,
    -- JSON array of response item IDs or content keys.
    item_keys TEXT NOT NULL,
    first_ordinal INTEGER NOT NULL,
    last_ordinal INTEGER NOT NULL,
    approx_bytes INTEGER NOT NULL,
    complete INTEGER NOT NULL,
    PRIMARY KEY (graph_id, block_id)
);

CREATE UNIQUE INDEX idx_cg_blocks_seq ON cg_blocks(graph_id, seq);

CREATE TABLE cg_edges (
    graph_id TEXT NOT NULL,
    child_block_id TEXT NOT NULL,
    -- 0 is the primary parent.
    position INTEGER NOT NULL,
    parent_block_id TEXT NOT NULL,
    PRIMARY KEY (graph_id, child_block_id, position)
);

CREATE INDEX idx_cg_edges_parent ON cg_edges(graph_id, parent_block_id);
