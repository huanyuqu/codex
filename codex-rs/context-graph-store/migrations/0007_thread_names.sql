-- Names given to a thread's branch, kept by thread so a name given before the thread first
-- records still applies when its branch is created.
CREATE TABLE cg_thread_names (
    thread_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL
);
