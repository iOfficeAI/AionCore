ALTER TABLE assistant_definitions
    ADD COLUMN default_thought_level_mode TEXT NOT NULL DEFAULT 'auto'
        CHECK (default_thought_level_mode IN ('auto', 'fixed'));

ALTER TABLE assistant_definitions
    ADD COLUMN default_thought_level_value TEXT;

ALTER TABLE assistant_preferences
    ADD COLUMN last_thought_level_value TEXT;

ALTER TABLE conversation_assistant_snapshots
    ADD COLUMN default_thought_level_mode TEXT NOT NULL DEFAULT 'auto'
        CHECK (default_thought_level_mode IN ('auto', 'fixed'));

ALTER TABLE conversation_assistant_snapshots
    ADD COLUMN resolved_thought_level_value TEXT;

-- Remove retired runtime configuration/cache blobs from the generic client
-- preference store so they cannot affect the assistant-default model.
DELETE FROM client_preferences
WHERE key IN (
    'acp.config',
    'aionrs.config',
    'codex.config',
    'acp.cachedModes',
    'acp.cachedInitializeResult',
    'acp.cached_config_options'
);
