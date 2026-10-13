-- The block that holds a merge's note in the target branch, whose second parent is the source
-- branch's head.
ALTER TABLE cg_merges ADD COLUMN merge_block TEXT;
