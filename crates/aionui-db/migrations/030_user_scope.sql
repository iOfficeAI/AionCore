-- Migration 030: add user scope for local, aggregate, and configuration data.

PRAGMA foreign_keys = OFF;

CREATE TABLE users_new (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_type          TEXT NOT NULL DEFAULT 'local'
                           CHECK(user_type IN ('local', 'aionpro')),
    external_user_id   TEXT,
    username           TEXT,
    email              TEXT,
    password_hash      TEXT,
    avatar_path        TEXT,
    jwt_secret         TEXT,
    status             TEXT NOT NULL DEFAULT 'active'
                           CHECK(status IN ('active', 'disabled')),
    session_generation INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    last_login         INTEGER,
    CHECK (
        (user_type = 'local' AND password_hash IS NOT NULL)
        OR
        (user_type = 'aionpro')
    ),
    CHECK (
        (external_user_id IS NULL)
        OR
        (length(external_user_id) > 0)
    )
);

INSERT INTO users_new (
    id,
    user_type,
    external_user_id,
    username,
    email,
    password_hash,
    avatar_path,
    jwt_secret,
    status,
    session_generation,
    created_at,
    updated_at,
    last_login
)
SELECT
    id,
    'local',
    NULL,
    username,
    email,
    password_hash,
    avatar_path,
    jwt_secret,
    'active',
    0,
    created_at,
    updated_at,
    last_login
FROM users;

CREATE TEMP TABLE user_scope_migration_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM users_new) = (SELECT COUNT(*) FROM users)
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM messages m
        LEFT JOIN conversations c ON c.id = m.conversation_id
        WHERE c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM conversation_artifacts a
        LEFT JOIN conversations c ON c.id = a.conversation_id
        WHERE c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM conversation_assistant_snapshots s
        LEFT JOIN conversations c ON c.id = s.conversation_id
        WHERE c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM acp_session s
        LEFT JOIN conversations c ON c.id = s.conversation_id
        WHERE c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM cron_jobs j
        LEFT JOIN conversations c ON c.id = j.conversation_id
        WHERE COALESCE(j.conversation_id, '') <> ''
          AND c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM mailbox m
        LEFT JOIN teams t ON t.id = m.team_id
        WHERE t.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_migration_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM team_tasks tt
        LEFT JOIN teams t ON t.id = tt.team_id
        WHERE t.id IS NULL
    )
    THEN 1
    ELSE 0
END;

DROP TABLE user_scope_migration_checks;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE UNIQUE INDEX idx_users_local_username
    ON users(username)
    WHERE user_type = 'local' AND username IS NOT NULL;
CREATE UNIQUE INDEX idx_users_email
    ON users(email)
    WHERE email IS NOT NULL;
CREATE UNIQUE INDEX idx_users_external_user
    ON users(user_type, external_user_id)
    WHERE external_user_id IS NOT NULL;
CREATE INDEX idx_users_username ON users(username);
CREATE INDEX idx_users_status ON users(status);

CREATE TRIGGER IF NOT EXISTS trg_mailbox_team_parent_insert
BEFORE INSERT ON mailbox
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'mailbox.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_mailbox_team_parent_update
BEFORE UPDATE OF team_id ON mailbox
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'mailbox.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_team_tasks_team_parent_insert
BEFORE INSERT ON team_tasks
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'team_tasks.team_id must reference teams.id');
END;

CREATE TRIGGER IF NOT EXISTS trg_team_tasks_team_parent_update
BEFORE UPDATE OF team_id ON team_tasks
FOR EACH ROW
WHEN NOT EXISTS (SELECT 1 FROM teams WHERE id = NEW.team_id)
BEGIN
    SELECT RAISE(ABORT, 'team_tasks.team_id must reference teams.id');
END;

ALTER TABLE cron_jobs
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user';
CREATE INDEX IF NOT EXISTS idx_cron_jobs_user_next_run ON cron_jobs(user_id, next_run_at);

ALTER TABLE providers
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user';
CREATE INDEX IF NOT EXISTS idx_providers_user_platform ON providers(user_id, platform);

ALTER TABLE remote_agents
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user';
CREATE INDEX IF NOT EXISTS idx_remote_agents_user_status ON remote_agents(user_id, status);

CREATE TEMP TABLE user_scope_rebuild_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

CREATE TABLE mcp_servers_new (
    id               TEXT PRIMARY KEY NOT NULL,
    user_id          TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    name             TEXT    NOT NULL,
    description      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0,
    transport_type   TEXT    NOT NULL,
    transport_config TEXT    NOT NULL,
    tools            TEXT,
    last_test_status TEXT    NOT NULL DEFAULT 'disconnected',
    last_connected   INTEGER,
    original_json    TEXT,
    builtin          INTEGER NOT NULL DEFAULT 0,
    deleted_at       INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (user_id, name)
);

INSERT INTO mcp_servers_new (
    id, user_id, name, description, enabled, transport_type, transport_config,
    tools, last_test_status, last_connected, original_json, builtin, deleted_at,
    created_at, updated_at
)
SELECT
    id, 'system_default_user', name, description, enabled, transport_type,
    transport_config, tools, last_test_status, last_connected, original_json,
    builtin, deleted_at, created_at, updated_at
FROM mcp_servers;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM mcp_servers_new) = (SELECT COUNT(*) FROM mcp_servers)
    THEN 1
    ELSE 0
END;

DROP TABLE mcp_servers;
ALTER TABLE mcp_servers_new RENAME TO mcp_servers;
CREATE INDEX IF NOT EXISTS idx_mcp_servers_user_name ON mcp_servers(user_id, name);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_user_enabled ON mcp_servers(user_id, enabled);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_deleted_at ON mcp_servers(deleted_at);

CREATE TABLE oauth_tokens_new (
    user_id       TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    server_url    TEXT NOT NULL,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    token_type    TEXT NOT NULL DEFAULT 'bearer',
    expires_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, server_url)
);

INSERT INTO oauth_tokens_new (
    user_id, server_url, access_token, refresh_token, token_type, expires_at,
    created_at, updated_at
)
SELECT
    'system_default_user', server_url, access_token, refresh_token, token_type,
    expires_at, created_at, updated_at
FROM oauth_tokens;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM oauth_tokens_new) = (SELECT COUNT(*) FROM oauth_tokens)
    THEN 1
    ELSE 0
END;

DROP TABLE oauth_tokens;
ALTER TABLE oauth_tokens_new RENAME TO oauth_tokens;

CREATE TABLE system_settings_new (
    user_id                   TEXT PRIMARY KEY NOT NULL REFERENCES users(id),
    language                  TEXT    NOT NULL DEFAULT 'en-US',
    notification_enabled      INTEGER NOT NULL DEFAULT 1,
    cron_notification_enabled INTEGER NOT NULL DEFAULT 0,
    command_queue_enabled     INTEGER NOT NULL DEFAULT 0,
    save_upload_to_workspace  INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL
);

INSERT INTO system_settings_new (
    user_id, language, notification_enabled, cron_notification_enabled,
    command_queue_enabled, save_upload_to_workspace, updated_at
)
SELECT
    'system_default_user', language, notification_enabled,
    cron_notification_enabled, command_queue_enabled, save_upload_to_workspace,
    updated_at
FROM system_settings
WHERE id = 1;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM system_settings_new) = (SELECT COUNT(*) FROM system_settings WHERE id = 1)
    THEN 1
    ELSE 0
END;

DROP TABLE system_settings;
ALTER TABLE system_settings_new RENAME TO system_settings;

CREATE TABLE client_preferences_new (
    user_id    TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, key)
);

INSERT INTO client_preferences_new (user_id, key, value, updated_at)
SELECT 'system_default_user', key, value, updated_at
FROM client_preferences;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM client_preferences_new) = (SELECT COUNT(*) FROM client_preferences)
    THEN 1
    ELSE 0
END;

DROP TABLE client_preferences;
ALTER TABLE client_preferences_new RENAME TO client_preferences;

CREATE TABLE agent_metadata_new (
    -- Surrogate row key; the logical agent identity is agent_id, which is
    -- globally UNIQUE — the catalog holds exactly one row per agent. Builtin
    -- rows have user_id NULL; custom agents carry their creator's user_id as
    -- a pure ownership/visibility attribute. An agent is a machine-level
    -- resource (one CLI binary + login per device, shared by all Core Users),
    -- so this single row holds ALL of its state — capabilities, availability
    -- probe, command/env overrides, AND the enable/disable toggle (which gates
    -- whether the registry starts the agent, hence machine-level). There is no
    -- per-user agent state table; per-user enable/disable happens one layer up,
    -- on assistants (assistant_overrides).
    id                       TEXT PRIMARY KEY NOT NULL,
    agent_id                 TEXT NOT NULL,
    user_id                  TEXT REFERENCES users(id),
    icon                     TEXT,
    name                     TEXT NOT NULL,
    name_i18n                TEXT,
    description              TEXT,
    description_i18n         TEXT,
    backend                  TEXT,
    agent_type               TEXT NOT NULL,
    agent_source             TEXT NOT NULL,
    agent_source_info        TEXT,
    enabled                  INTEGER NOT NULL DEFAULT 1,
    command                  TEXT,
    args                     TEXT,
    env                      TEXT,
    native_skills_dirs       TEXT,
    behavior_policy          TEXT,
    yolo_id                  TEXT,
    agent_capabilities       TEXT,
    auth_methods             TEXT,
    config_options           TEXT,
    available_modes          TEXT,
    available_models         TEXT,
    available_commands       TEXT,
    sort_order               INTEGER NOT NULL DEFAULT 1000,
    last_check_status        TEXT,
    last_check_kind          TEXT,
    last_check_error_code    TEXT,
    last_check_error_message TEXT,
    last_check_guidance      TEXT,
    last_check_latency_ms    INTEGER,
    last_check_at            INTEGER,
    last_success_at          INTEGER,
    last_failure_at          INTEGER,
    command_override         TEXT,
    env_override             TEXT,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL
);

INSERT INTO agent_metadata_new (
    id, agent_id, user_id, icon, name, name_i18n, description, description_i18n,
    backend, agent_type, agent_source, agent_source_info,
    enabled, command, args, env, native_skills_dirs,
    behavior_policy, yolo_id,
    agent_capabilities, auth_methods, config_options,
    available_modes, available_models, available_commands,
    sort_order,
    last_check_status, last_check_kind, last_check_error_code, last_check_error_message,
    last_check_guidance, last_check_latency_ms, last_check_at, last_success_at, last_failure_at,
    command_override, env_override, created_at, updated_at
)
SELECT
    -- Pre-migration ids are unique (old primary key), so they can seed both
    -- the surrogate row key and the logical agent id.
    id,
    id,
    CASE WHEN agent_source IN ('builtin', 'internal') THEN NULL ELSE 'system_default_user' END,
    icon, name, name_i18n, description, description_i18n,
    backend, agent_type, agent_source, agent_source_info,
    enabled, command, args, env, native_skills_dirs,
    behavior_policy, yolo_id,
    agent_capabilities, auth_methods, config_options,
    available_modes, available_models, available_commands,
    sort_order,
    last_check_status, last_check_kind, last_check_error_code, last_check_error_message,
    last_check_guidance, last_check_latency_ms, last_check_at, last_success_at, last_failure_at,
    command_override, env_override, created_at, updated_at
FROM agent_metadata;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM agent_metadata_new) = (SELECT COUNT(*) FROM agent_metadata)
    THEN 1
    ELSE 0
END;

DROP TABLE agent_metadata;
ALTER TABLE agent_metadata_new RENAME TO agent_metadata;
CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_metadata_agent_id_unique
    ON agent_metadata(agent_id);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_backend ON agent_metadata(backend);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_agent_type ON agent_metadata(agent_type);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_sort_order ON agent_metadata(sort_order);

-- No per-user agent table: agents are machine-level (see agent_metadata note
-- above). Per-user enable/disable lives on assistants via assistant_overrides.
CREATE INDEX IF NOT EXISTS idx_agent_metadata_user_sort
    ON agent_metadata(user_id, sort_order, name);

ALTER TABLE assistants
    ADD COLUMN user_id TEXT REFERENCES users(id);
UPDATE assistants
SET user_id = 'system_default_user'
WHERE user_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_assistants_user_updated_at
    ON assistants(user_id, updated_at DESC);

CREATE TABLE assistant_overrides_new (
    user_id           TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_id      TEXT NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    last_used_at      INTEGER,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (user_id, assistant_id)
);

INSERT INTO assistant_overrides_new (
    user_id, assistant_id, enabled, sort_order, last_used_at, updated_at
)
SELECT
    'system_default_user', assistant_id, enabled, sort_order, last_used_at, updated_at
FROM assistant_overrides;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_overrides_new) = (SELECT COUNT(*) FROM assistant_overrides)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_overrides;
ALTER TABLE assistant_overrides_new RENAME TO assistant_overrides;
CREATE INDEX IF NOT EXISTS idx_assistant_overrides_user_sort
    ON assistant_overrides(user_id, sort_order);

DROP INDEX IF EXISTS idx_assistant_definitions_source_ref;
DROP INDEX IF EXISTS idx_assistant_definitions_assistant_id;

CREATE TABLE assistant_definitions_new (
    id                                   TEXT PRIMARY KEY NOT NULL,
    user_id                              TEXT REFERENCES users(id),
    assistant_id                         TEXT NOT NULL,
    source                               TEXT NOT NULL
                                             CHECK (source IN ('builtin', 'user', 'generated')),
    owner_type                           TEXT NOT NULL
                                             CHECK (owner_type IN ('system', 'user')),
    source_ref                           TEXT,
    name                                 TEXT NOT NULL,
    name_i18n                            TEXT NOT NULL DEFAULT '{}',
    description                          TEXT,
    description_i18n                     TEXT NOT NULL DEFAULT '{}',
    avatar_type                          TEXT NOT NULL DEFAULT 'none'
                                             CHECK (avatar_type IN ('none', 'emoji', 'builtin_asset', 'user_asset')),
    avatar_value                         TEXT,
    agent_id                             TEXT NOT NULL,
    rule_resource_type                   TEXT NOT NULL
                                             CHECK (rule_resource_type IN ('none', 'builtin_asset', 'user_file', 'extension')),
    rule_resource_ref                    TEXT,
    recommended_prompts                  TEXT NOT NULL DEFAULT '[]',
    recommended_prompts_i18n             TEXT NOT NULL DEFAULT '{}',
    default_model_mode                   TEXT NOT NULL
                                             CHECK (default_model_mode IN ('auto', 'fixed')),
    default_model_value                  TEXT,
    default_permission_mode              TEXT NOT NULL
                                             CHECK (default_permission_mode IN ('auto', 'fixed')),
    default_permission_value             TEXT,
    default_thought_level_mode           TEXT NOT NULL DEFAULT 'auto'
                                             CHECK (default_thought_level_mode IN ('auto', 'fixed')),
    default_thought_level_value          TEXT,
    default_skills_mode                  TEXT NOT NULL
                                             CHECK (default_skills_mode IN ('auto', 'fixed')),
    default_skill_ids                    TEXT NOT NULL DEFAULT '[]',
    custom_skill_names                   TEXT NOT NULL DEFAULT '[]',
    default_disabled_builtin_skill_ids   TEXT NOT NULL DEFAULT '[]',
    default_mcps_mode                    TEXT NOT NULL
                                             CHECK (default_mcps_mode IN ('auto', 'fixed')),
    default_mcp_ids                      TEXT NOT NULL DEFAULT '[]',
    created_at                           INTEGER NOT NULL,
    updated_at                           INTEGER NOT NULL,
    deleted_at                           INTEGER
);

INSERT INTO assistant_definitions_new (
    id, user_id, assistant_id, source, owner_type, source_ref,
    name, name_i18n, description, description_i18n, avatar_type, avatar_value,
    agent_id, rule_resource_type, rule_resource_ref,
    recommended_prompts, recommended_prompts_i18n,
    default_model_mode, default_model_value,
    default_permission_mode, default_permission_value,
    default_thought_level_mode, default_thought_level_value,
    default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
    default_mcps_mode, default_mcp_ids,
    created_at, updated_at, deleted_at
)
SELECT
    id,
    CASE
        WHEN source = 'builtin' AND owner_type = 'system' THEN NULL
        ELSE 'system_default_user'
    END,
    assistant_id, source, owner_type, source_ref,
    name, name_i18n, description, description_i18n, avatar_type, avatar_value,
    agent_id, rule_resource_type, rule_resource_ref,
    recommended_prompts, recommended_prompts_i18n,
    default_model_mode, default_model_value,
    default_permission_mode, default_permission_value,
    default_thought_level_mode, default_thought_level_value,
    default_skills_mode, default_skill_ids, custom_skill_names, default_disabled_builtin_skill_ids,
    default_mcps_mode, default_mcp_ids,
    created_at, updated_at, deleted_at
FROM assistant_definitions;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_definitions_new) = (SELECT COUNT(*) FROM assistant_definitions)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_definitions;
ALTER TABLE assistant_definitions_new RENAME TO assistant_definitions;

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_global_source_ref
    ON assistant_definitions(source, source_ref)
    WHERE user_id IS NULL AND source_ref IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_user_source_ref
    ON assistant_definitions(user_id, source, source_ref)
    WHERE user_id IS NOT NULL AND source_ref IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_global_assistant_id
    ON assistant_definitions(assistant_id)
    WHERE user_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_user_assistant_id
    ON assistant_definitions(user_id, assistant_id)
    WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_assistant_definitions_source
    ON assistant_definitions(source);
CREATE INDEX IF NOT EXISTS idx_assistant_definitions_agent_id
    ON assistant_definitions(agent_id);
CREATE INDEX IF NOT EXISTS idx_assistant_definitions_user_updated_at
    ON assistant_definitions(user_id, updated_at DESC);

CREATE TABLE assistant_overlays_new (
    user_id                 TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_definition_id TEXT NOT NULL,
    enabled                 INTEGER NOT NULL DEFAULT 1,
    sort_order              INTEGER NOT NULL DEFAULT 0,
    agent_id_override       TEXT,
    last_used_at            INTEGER,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    PRIMARY KEY (user_id, assistant_definition_id),
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(id) ON DELETE CASCADE
);

INSERT INTO assistant_overlays_new (
    user_id, assistant_definition_id, enabled, sort_order, agent_id_override,
    last_used_at, created_at, updated_at
)
SELECT
    'system_default_user', assistant_definition_id, enabled, sort_order,
    agent_id_override, last_used_at, created_at, updated_at
FROM assistant_overlays;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_overlays_new) = (SELECT COUNT(*) FROM assistant_overlays)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_overlays;
ALTER TABLE assistant_overlays_new RENAME TO assistant_overlays;
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_user_enabled
    ON assistant_overlays(user_id, enabled);
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_user_sort_order
    ON assistant_overlays(user_id, sort_order);
CREATE INDEX IF NOT EXISTS idx_assistant_overlays_agent_id_override
    ON assistant_overlays(agent_id_override)
    WHERE agent_id_override IS NOT NULL;

CREATE TABLE assistant_preferences_new (
    user_id                              TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    assistant_definition_id              TEXT NOT NULL,
    last_model_id                        TEXT,
    last_permission_value                TEXT,
    last_skill_ids                       TEXT    NOT NULL DEFAULT '[]',
    last_disabled_builtin_skill_ids      TEXT    NOT NULL DEFAULT '[]',
    last_mcp_ids                         TEXT    NOT NULL DEFAULT '[]',
    created_at                           INTEGER NOT NULL,
    updated_at                           INTEGER NOT NULL,
    last_thought_level_value             TEXT,
    PRIMARY KEY (user_id, assistant_definition_id),
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(id) ON DELETE CASCADE
);

INSERT INTO assistant_preferences_new (
    user_id, assistant_definition_id, last_model_id, last_permission_value,
    last_skill_ids, last_disabled_builtin_skill_ids, last_mcp_ids,
    created_at, updated_at, last_thought_level_value
)
SELECT
    'system_default_user', assistant_definition_id, last_model_id, last_permission_value,
    last_skill_ids, last_disabled_builtin_skill_ids, last_mcp_ids,
    created_at, updated_at, last_thought_level_value
FROM assistant_preferences;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_preferences_new) = (SELECT COUNT(*) FROM assistant_preferences)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_preferences;
ALTER TABLE assistant_preferences_new RENAME TO assistant_preferences;

CREATE TABLE skills_new (
    id          TEXT    PRIMARY KEY NOT NULL,
    user_id     TEXT    REFERENCES users(id),
    name        TEXT    NOT NULL,
    description TEXT,
    path        TEXT    NOT NULL,
    source      TEXT    NOT NULL DEFAULT 'user'
                            CHECK (source IN ('user', 'builtin', 'extension', 'cron')),
    enabled     INTEGER NOT NULL DEFAULT 1,
    deleted_at  INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

INSERT INTO skills_new (
    id, user_id, name, description, path, source, enabled, deleted_at, created_at, updated_at
)
SELECT
    id,
    CASE WHEN source = 'builtin' THEN NULL ELSE 'system_default_user' END,
    name, description, path, source, enabled, deleted_at, created_at, updated_at
FROM skills;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM skills_new) = (SELECT COUNT(*) FROM skills)
    THEN 1
    ELSE 0
END;

DROP TABLE skills;
ALTER TABLE skills_new RENAME TO skills;
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_global_name
    ON skills(name)
    WHERE user_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_skills_user_name
    ON skills(user_id, name)
    WHERE user_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_skills_user_deleted_at ON skills(user_id, deleted_at);
CREATE INDEX IF NOT EXISTS idx_skills_source ON skills(source);
CREATE INDEX IF NOT EXISTS idx_skills_updated_at ON skills(updated_at DESC);

ALTER TABLE skill_import_records
    ADD COLUMN user_id TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id);
CREATE INDEX IF NOT EXISTS idx_skill_import_records_user_created_at
    ON skill_import_records(user_id, created_at DESC);

CREATE TABLE assistant_plugins_new (
    id             TEXT    NOT NULL,
    owner_user_id  TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    type           TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0,
    config         TEXT    NOT NULL,
    status         TEXT,
    last_connected INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (owner_user_id, id)
);

INSERT INTO assistant_plugins_new (
    id, owner_user_id, type, name, enabled, config, status, last_connected,
    created_at, updated_at
)
SELECT
    id, 'system_default_user', type, name, enabled, config, status,
    last_connected, created_at, updated_at
FROM assistant_plugins;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_plugins_new) = (SELECT COUNT(*) FROM assistant_plugins)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_plugins;
ALTER TABLE assistant_plugins_new RENAME TO assistant_plugins;
CREATE INDEX IF NOT EXISTS idx_assistant_plugins_owner_created_at
    ON assistant_plugins(owner_user_id, created_at ASC);

CREATE TABLE assistant_users_new (
    id               TEXT PRIMARY KEY NOT NULL,
    owner_user_id    TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    platform_user_id TEXT NOT NULL,
    platform_type    TEXT NOT NULL,
    display_name     TEXT,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    session_id       TEXT,
    UNIQUE (owner_user_id, platform_user_id, platform_type)
);

INSERT INTO assistant_users_new (
    id, owner_user_id, platform_user_id, platform_type, display_name,
    authorized_at, last_active, session_id
)
SELECT
    id, 'system_default_user', platform_user_id, platform_type, display_name,
    authorized_at, last_active, session_id
FROM assistant_users;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_users_new) = (SELECT COUNT(*) FROM assistant_users)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_users;
ALTER TABLE assistant_users_new RENAME TO assistant_users;
CREATE INDEX IF NOT EXISTS idx_assistant_users_owner_authorized_at
    ON assistant_users(owner_user_id, authorized_at DESC);

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM assistant_sessions s
        LEFT JOIN assistant_users u ON u.id = s.user_id
        WHERE u.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM assistant_sessions s
        LEFT JOIN conversations c ON c.id = s.conversation_id
        WHERE s.conversation_id IS NOT NULL
          AND c.id IS NULL
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM assistant_sessions s
        JOIN assistant_users u ON u.id = s.user_id
        JOIN conversations c ON c.id = s.conversation_id
        WHERE s.conversation_id IS NOT NULL
          AND c.user_id != u.owner_user_id
    )
    THEN 1
    ELSE 0
END;

CREATE TABLE assistant_pairing_codes_new (
    code             TEXT NOT NULL,
    owner_user_id    TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    platform_user_id TEXT NOT NULL,
    platform_type    TEXT NOT NULL,
    display_name     TEXT,
    requested_at     INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    PRIMARY KEY (owner_user_id, code)
);

INSERT INTO assistant_pairing_codes_new (
    code, owner_user_id, platform_user_id, platform_type, display_name,
    requested_at, expires_at, status
)
SELECT
    code, 'system_default_user', platform_user_id, platform_type, display_name,
    requested_at, expires_at, status
FROM assistant_pairing_codes;

INSERT INTO user_scope_rebuild_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_pairing_codes_new) = (SELECT COUNT(*) FROM assistant_pairing_codes)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_pairing_codes;
ALTER TABLE assistant_pairing_codes_new RENAME TO assistant_pairing_codes;
CREATE INDEX IF NOT EXISTS idx_pairing_codes_owner_status
    ON assistant_pairing_codes(owner_user_id, status);

-- ---------------------------------------------------------------------------
-- Project-bind tables (created by 028_project_bind, which precedes this
-- migration). `projects` holds user data (user-named projects, user-selected
-- workspace roots; temp projects follow user-owned conversations), so it is
-- an independent user-scoped root. `folders` stays global by design: a
-- canonical filesystem path is a machine fact, deduped across users.
-- `project_explorer` inherits its user through the project parent chain but
-- carries a denormalized owner so the one-workspace-per-folder uniqueness
-- can be expressed per user (SQLite cannot index across tables) — same
-- pattern as the temporary Channel owner fields.
--
-- Both tables are guaranteed empty here (the feature is un-wired in 028's
-- phase 1), so the rebuilds carry no data. The DEFAULT keeps phase-1 store
-- code (which does not write owner columns) working unchanged; phase 2 must
-- write owners explicitly and resolve workspace entries per user.
-- ---------------------------------------------------------------------------

CREATE TEMP TABLE user_scope_project_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

CREATE TABLE projects_new (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT    NOT NULL,
    user_id    TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    name       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

INSERT INTO projects_new (id, project_id, user_id, name, kind, created_at, updated_at)
SELECT id, project_id, 'system_default_user', name, kind, created_at, updated_at
FROM projects;

INSERT INTO user_scope_project_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM projects_new) = (SELECT COUNT(*) FROM projects)
    THEN 1
    ELSE 0
END;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_project_id_unique ON projects(project_id);
CREATE INDEX IF NOT EXISTS idx_projects_user_updated ON projects(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_projects_user_kind_updated ON projects(user_id, kind, updated_at DESC);

CREATE TABLE project_explorer_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    pe_id         TEXT    NOT NULL,
    project_id    TEXT    NOT NULL,
    owner_user_id TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    folder_id     TEXT    NOT NULL,
    role          TEXT    NOT NULL,
    display_name  TEXT,
    order_index   INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

INSERT INTO project_explorer_new (
    id, pe_id, project_id, owner_user_id, folder_id, role, display_name, order_index, created_at, updated_at
)
SELECT
    id, pe_id, project_id, 'system_default_user', folder_id, role, display_name, order_index, created_at, updated_at
FROM project_explorer;

INSERT INTO user_scope_project_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM project_explorer_new) = (SELECT COUNT(*) FROM project_explorer)
    THEN 1
    ELSE 0
END;

DROP TABLE project_explorer;
ALTER TABLE project_explorer_new RENAME TO project_explorer;

CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_pe_id_unique ON project_explorer(pe_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_project_folder_unique
    ON project_explorer(project_id, folder_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_one_workspace
    ON project_explorer(project_id) WHERE role = 'workspace';
-- Per user: a folder can be the workspace root of at most one project PER
-- OWNER. Replaces 028's global (folder_id) uniqueness, which would let one
-- user permanently reserve a directory against every other user.
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_one_workspace_folder
    ON project_explorer(owner_user_id, folder_id) WHERE role = 'workspace';
CREATE INDEX IF NOT EXISTS idx_project_explorer_project_order ON project_explorer(project_id, order_index);
CREATE INDEX IF NOT EXISTS idx_project_explorer_folder_id ON project_explorer(folder_id);

-- Cross-scope integrity: explorer owner must match its project's owner, and
-- user-scoped roots referencing a project must share its owner. All tables
-- involved are empty at migration time; these guard future upgrade paths.
INSERT INTO user_scope_project_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM project_explorer pe
        JOIN projects p ON p.project_id = pe.project_id
        WHERE pe.owner_user_id != p.user_id
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_project_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM conversations c
        JOIN projects p ON p.project_id = c.project_id
        WHERE c.project_id IS NOT NULL
          AND p.user_id != c.user_id
    )
    THEN 1
    ELSE 0
END;

INSERT INTO user_scope_project_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1
        FROM teams t
        JOIN projects p ON p.project_id = t.project_id
        WHERE t.project_id IS NOT NULL
          AND p.user_id != t.user_id
    )
    THEN 1
    ELSE 0
END;

DROP TABLE user_scope_project_checks;

DROP TABLE user_scope_rebuild_checks;

PRAGMA foreign_keys = ON;
