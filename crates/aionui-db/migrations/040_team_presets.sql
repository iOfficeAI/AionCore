-- Migration 040: Add team_presets table for persisted expert-team presets.
-- (rebased onto upstream max 038 for AionCore v0.1.67; content from former local 039)

CREATE TABLE IF NOT EXISTS team_presets (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    icon            TEXT,
    category        TEXT,
    description     TEXT    NOT NULL,
    expertise_tags  TEXT    NOT NULL DEFAULT '[]',
    example_prompts TEXT    NOT NULL DEFAULT '[]',
    leader          TEXT    NOT NULL,
    members         TEXT    NOT NULL DEFAULT '[]',
    version         INTEGER NOT NULL DEFAULT 1,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_team_presets_user_id
    ON team_presets(user_id);

CREATE INDEX IF NOT EXISTS idx_team_presets_user_updated_at
    ON team_presets(user_id, updated_at DESC);
