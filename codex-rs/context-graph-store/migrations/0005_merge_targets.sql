-- Where the target branch was when a branch was merged into it. Recall places the blocks of a
-- referenced branch after this block of the target's history.
ALTER TABLE cg_merges ADD COLUMN target_head TEXT;
