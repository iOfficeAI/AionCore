-- Migration 012: assistant data unification
--
-- Introduces the new assistant runtime storage:
--   - assistant_definitions
--   - assistant_overlays
--   - assistant_preferences
--   - conversation_assistant_snapshots
--
-- Legacy tables `assistants` and `assistant_overrides` are intentionally kept
-- for downgrade compatibility and mirror projection.

CREATE TABLE IF NOT EXISTS assistant_definitions (
    definition_id                      TEXT PRIMARY KEY,
    assistant_key                      TEXT    NOT NULL,
    source                             TEXT    NOT NULL
                                               CHECK (source IN ('builtin', 'user', 'generated')),
    owner_type                         TEXT    NOT NULL
                                               CHECK (owner_type IN ('system', 'user')),
    source_ref                         TEXT,
    source_version                     TEXT,
    source_hash                        TEXT,
    name                               TEXT    NOT NULL,
    name_i18n                          TEXT    NOT NULL DEFAULT '{}',
    description                        TEXT,
    description_i18n                   TEXT    NOT NULL DEFAULT '{}',
    avatar_type                        TEXT    NOT NULL DEFAULT 'none'
                                               CHECK (avatar_type IN ('none', 'emoji', 'builtin_asset', 'user_asset')),
    avatar_value                       TEXT,
    agent_backend                      TEXT    NOT NULL,
    rule_resource_type                 TEXT    NOT NULL
                                               CHECK (
                                                   rule_resource_type IN (
                                                       'none',
                                                       'builtin_asset',
                                                       'user_file',
                                                       'inline',
                                                       'extension'
                                                   )
                                               ),
    rule_resource_ref                  TEXT,
    rule_inline_content                TEXT,
    recommended_prompts                TEXT    NOT NULL DEFAULT '[]',
    recommended_prompts_i18n           TEXT    NOT NULL DEFAULT '{}',
    default_model_mode                 TEXT    NOT NULL
                                               CHECK (default_model_mode IN ('auto', 'fixed')),
    default_model_value                TEXT,
    default_permission_mode            TEXT    NOT NULL
                                               CHECK (default_permission_mode IN ('auto', 'fixed')),
    default_permission_value           TEXT,
    default_skills_mode                TEXT    NOT NULL
                                               CHECK (default_skills_mode IN ('auto', 'fixed')),
    default_skill_ids                  TEXT    NOT NULL DEFAULT '[]',
    custom_skill_names                 TEXT    NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids TEXT    NOT NULL DEFAULT '[]',
    default_mcps_mode                  TEXT    NOT NULL
                                               CHECK (default_mcps_mode IN ('auto', 'fixed')),
    default_mcp_ids                    TEXT    NOT NULL DEFAULT '[]',
    created_at                         INTEGER NOT NULL,
    updated_at                         INTEGER NOT NULL,
    deleted_at                         INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_source_ref
    ON assistant_definitions(source, source_ref)
    WHERE source_ref IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_assistant_key
    ON assistant_definitions(assistant_key);

CREATE INDEX IF NOT EXISTS idx_assistant_definitions_source
    ON assistant_definitions(source);

CREATE INDEX IF NOT EXISTS idx_assistant_definitions_agent_backend
    ON assistant_definitions(agent_backend);

CREATE TABLE IF NOT EXISTS assistant_overlays (
    definition_id TEXT PRIMARY KEY,
    enabled       INTEGER NOT NULL DEFAULT 1,
    sort_order    INTEGER NOT NULL DEFAULT 0,
    agent_backend_override TEXT,
    last_used_at  INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    FOREIGN KEY (definition_id) REFERENCES assistant_definitions(definition_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_assistant_overlays_enabled
    ON assistant_overlays(enabled);

CREATE INDEX IF NOT EXISTS idx_assistant_overlays_sort_order
    ON assistant_overlays(sort_order);

CREATE TABLE IF NOT EXISTS assistant_preferences (
    definition_id                      TEXT PRIMARY KEY,
    last_model_id                      TEXT,
    last_permission_value              TEXT,
    last_skill_ids                     TEXT    NOT NULL DEFAULT '[]',
    last_disabled_builtin_skill_ids    TEXT    NOT NULL DEFAULT '[]',
    last_mcp_ids                       TEXT    NOT NULL DEFAULT '[]',
    created_at                         INTEGER NOT NULL,
    updated_at                         INTEGER NOT NULL,
    FOREIGN KEY (definition_id) REFERENCES assistant_definitions(definition_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS conversation_assistant_snapshots (
    conversation_id                        TEXT PRIMARY KEY,
    assistant_definition_id                TEXT    NOT NULL,
    assistant_key                          TEXT    NOT NULL,
    assistant_source                       TEXT    NOT NULL,
    assistant_name                         TEXT    NOT NULL,
    assistant_avatar_type                  TEXT    NOT NULL DEFAULT 'none'
                                                   CHECK (
                                                       assistant_avatar_type IN (
                                                           'none',
                                                           'emoji',
                                                           'builtin_asset',
                                                           'user_asset'
                                                       )
                                                   ),
    assistant_avatar_value                 TEXT,
    agent_backend                          TEXT    NOT NULL,
    rules_content                          TEXT    NOT NULL DEFAULT '',
    default_model_mode                     TEXT    NOT NULL
                                                   CHECK (default_model_mode IN ('auto', 'fixed')),
    resolved_model_id                      TEXT,
    default_permission_mode                TEXT    NOT NULL
                                                   CHECK (default_permission_mode IN ('auto', 'fixed')),
    resolved_permission_value              TEXT,
    default_skills_mode                    TEXT    NOT NULL
                                                   CHECK (default_skills_mode IN ('auto', 'fixed')),
    resolved_skill_ids                     TEXT    NOT NULL DEFAULT '[]',
    resolved_disabled_builtin_skill_ids    TEXT    NOT NULL DEFAULT '[]',
    default_mcps_mode                      TEXT    NOT NULL
                                                   CHECK (default_mcps_mode IN ('auto', 'fixed')),
    resolved_mcp_ids                       TEXT    NOT NULL DEFAULT '[]',
    created_at                             INTEGER NOT NULL,
    updated_at                             INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(definition_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_conversation_assistant_snapshots_assistant_definition_id
    ON conversation_assistant_snapshots(assistant_definition_id);
