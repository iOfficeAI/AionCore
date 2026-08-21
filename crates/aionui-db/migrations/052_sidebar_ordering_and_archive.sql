-- Sidebar ordering foundation + archive columns (feature: left-panel grouping).
--
-- `user_order` is the source of truth for user-defined ordering across the
-- sidebar. It is a generic ordering table keyed by (user_id, scene, item_type,
-- item_id); v1 uses only the 'pinned' scene, where a row's *existence* means
-- the item is pinned (there is no boolean pinned column — the legacy
-- conversations.pinned / pinned_at columns are deprecated and left untouched
-- from here on). order_key drives intra-scene ordering; it is deliberately NOT
-- unique per (user_id, scene): a rebalance transaction may transiently collide
-- an updated key with an as-yet-unchanged row, and cursors tie-break on the
-- full (order_key, item_type, item_id) triple, so uniqueness buys nothing.
--
-- No pin backfill: pinned state is a user preference and is intentionally not
-- migrated from the deprecated columns or team localStorage (product decision,
-- 2026-08-11) — consistent with team pins never being migrated.
--
-- archived_at (NULL = not archived) is added here in one shot so PR-B (archive)
-- needs no further migration. The partial indexes cover the selective
-- "archived only" read path; the default sidebar path filters archived_at IS
-- NULL, which is non-selective and needs no index.

CREATE TABLE IF NOT EXISTS user_order (
    user_id    TEXT    NOT NULL,
    scene      TEXT    NOT NULL,            -- closed enum, v1 only 'pinned'
    item_type  TEXT    NOT NULL,            -- 'conversation' | 'team'
    item_id    TEXT    NOT NULL,
    order_key  INTEGER NOT NULL,            -- i64, not unique within a scene
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, scene, item_type, item_id)
);

CREATE INDEX IF NOT EXISTS idx_user_order_scene
    ON user_order(user_id, scene, order_key);

ALTER TABLE conversations ADD COLUMN archived_at INTEGER;  -- NULL = not archived
ALTER TABLE teams         ADD COLUMN archived_at INTEGER;  -- NULL = not archived

CREATE INDEX IF NOT EXISTS idx_conversations_archived
    ON conversations(user_id, archived_at) WHERE archived_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_teams_archived
    ON teams(user_id, archived_at) WHERE archived_at IS NOT NULL;
