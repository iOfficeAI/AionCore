-- Migration 039: Add origin_conversation_id to teams for ad-hoc teams from conversations
-- (rebased onto upstream max 038 for AionCore v0.1.67; content from former local 038)

ALTER TABLE teams ADD COLUMN origin_conversation_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_teams_origin_conversation_id
    ON teams(origin_conversation_id);

CREATE INDEX IF NOT EXISTS idx_teams_user_origin_conversation
    ON teams(user_id, origin_conversation_id);
