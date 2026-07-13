-- Migration 001: Consolidated initial schema for aionui-backend
--
-- This is the baseline schema for databases copied from the legacy
-- Electron-managed aionui.db (v26). It creates all tables, indexes,
-- and performs data normalization for fields that changed format
-- between the TypeScript and Rust eras.

------------------------------------------------------------------------
-- Core tables
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY NOT NULL,
    username      TEXT NOT NULL UNIQUE,
    email         TEXT UNIQUE,
    password_hash TEXT NOT NULL,
    avatar_path   TEXT,
    jwt_secret    TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    last_login    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_users_username ON users(username);
CREATE INDEX IF NOT EXISTS idx_users_email ON users(email);

CREATE TABLE IF NOT EXISTS system_settings (
    id                        INTEGER PRIMARY KEY CHECK (id = 1),
    language                  TEXT    NOT NULL DEFAULT 'en-US',
    notification_enabled      INTEGER NOT NULL DEFAULT 1,
    cron_notification_enabled INTEGER NOT NULL DEFAULT 0,
    command_queue_enabled     INTEGER NOT NULL DEFAULT 0,
    save_upload_to_workspace  INTEGER NOT NULL DEFAULT 0,
    updated_at                INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS client_preferences (
    key        TEXT PRIMARY KEY NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS providers (
    id                TEXT PRIMARY KEY NOT NULL,
    platform          TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    base_url          TEXT    NOT NULL,
    api_key_encrypted TEXT    NOT NULL,
    models            TEXT    NOT NULL DEFAULT '[]',
    enabled           INTEGER NOT NULL DEFAULT 1,
    capabilities      TEXT    NOT NULL DEFAULT '[]',
    context_limit     INTEGER,
    model_protocols   TEXT,
    model_enabled     TEXT,
    model_health      TEXT,
    bedrock_config    TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_providers_platform ON providers(platform);

------------------------------------------------------------------------
-- Conversations & Messages
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    type            TEXT    NOT NULL,
    extra           TEXT    NOT NULL DEFAULT '{}',
    model           TEXT,
    status          TEXT    NOT NULL DEFAULT 'pending'
                            CHECK(status IN ('pending', 'running', 'finished')),
    source          TEXT,
    channel_chat_id TEXT,
    pinned          INTEGER NOT NULL DEFAULT 0,
    pinned_at       INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_conversations_user_id ON conversations(user_id);
CREATE INDEX IF NOT EXISTS idx_conversations_updated_at ON conversations(updated_at);
CREATE INDEX IF NOT EXISTS idx_conversations_type ON conversations(type);
CREATE INDEX IF NOT EXISTS idx_conversations_user_updated ON conversations(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source ON conversations(source);
CREATE INDEX IF NOT EXISTS idx_conversations_source_updated ON conversations(source, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_source_chat ON conversations(source, channel_chat_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_conversations_cron_job_id ON conversations(json_extract(extra, '$.cronJobId'));

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT    PRIMARY KEY NOT NULL,
    conversation_id TEXT    NOT NULL,
    msg_id          TEXT,
    type            TEXT    NOT NULL,
    content         TEXT    NOT NULL DEFAULT '{}',
    position        TEXT    CHECK(position IN ('left', 'right', 'center', 'pop')),
    status          TEXT    CHECK(status IN ('finish', 'pending', 'error', 'work')),
    hidden          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
CREATE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created ON messages(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created_desc ON messages(conversation_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_type_created ON messages(type, created_at DESC);

CREATE TABLE IF NOT EXISTS conversation_artifacts (
    id              TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT    NOT NULL,
    cron_job_id     TEXT,
    kind            TEXT    NOT NULL
                            CHECK(kind IN ('cron_trigger', 'skill_suggest')),
    status          TEXT    NOT NULL DEFAULT 'active'
                            CHECK(status IN ('active', 'pending', 'dismissed', 'saved')),
    payload         TEXT    NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_conversation_id ON conversation_artifacts(conversation_id);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_created_at ON conversation_artifacts(created_at);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_conversation_created ON conversation_artifacts(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_cron_job ON conversation_artifacts(cron_job_id);
CREATE INDEX IF NOT EXISTS idx_conversation_artifacts_kind_status ON conversation_artifacts(kind, status);

------------------------------------------------------------------------
-- ACP Sessions
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS acp_session (
    conversation_id TEXT PRIMARY KEY,
    agent_backend   TEXT    NOT NULL,
    agent_source    TEXT    NOT NULL,
    agent_id        TEXT    NOT NULL,
    session_id      TEXT,
    session_status  TEXT    NOT NULL DEFAULT 'idle',
    session_config  TEXT    NOT NULL DEFAULT '{}',
    last_active_at  INTEGER,
    suspended_at    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_acp_session_status ON acp_session(session_status);
CREATE INDEX IF NOT EXISTS idx_acp_session_suspended ON acp_session(session_status, suspended_at) WHERE session_status = 'suspended';
CREATE INDEX IF NOT EXISTS idx_acp_session_agent_id ON acp_session(agent_id);

------------------------------------------------------------------------
-- Agent Metadata
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS agent_metadata (
    id                  TEXT PRIMARY KEY NOT NULL,
    icon                TEXT,
    name                TEXT NOT NULL,
    name_i18n           TEXT,
    description         TEXT,
    description_i18n    TEXT,
    backend             TEXT,
    agent_type          TEXT NOT NULL,
    agent_source        TEXT NOT NULL,
    agent_source_info   TEXT,
    enabled             INTEGER NOT NULL DEFAULT 1,
    command             TEXT,
    args                TEXT,
    env                 TEXT,
    native_skills_dirs  TEXT,
    behavior_policy     TEXT,
    yolo_id             TEXT,
    agent_capabilities  TEXT,
    auth_methods        TEXT,
    config_options      TEXT,
    available_modes     TEXT,
    available_models    TEXT,
    available_commands  TEXT,
    sort_order          INTEGER NOT NULL DEFAULT 1000,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_backend ON agent_metadata(backend);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_agent_type ON agent_metadata(agent_type);
CREATE INDEX IF NOT EXISTS idx_agent_metadata_sort_order ON agent_metadata(sort_order);

-- Seed agent_metadata with builtin agents (final state: includes icon, sort_order, behavior_policy)
INSERT OR IGNORE INTO agent_metadata
    (id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    -- ACP builtin agents
    ('2d23ff1c', '/api/assets/logos/ai-major/claude.svg', 'Claude Code',
     'claude', 'acp', 'builtin', '{"binary_name":"claude","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@agentclientprotocol/claude-agent-acp@0.29.2"]', '[]',
     '[".claude/skills"]',
     '{"supports_side_question":true,"self_identity_sticky":true,"session_load_via_meta_field":true,"supports_team":true}',
     'bypassPermissions', 3100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('8e1acf31', '/api/assets/logos/tools/coding/codex.svg', 'Codex CLI',
     'codex', 'acp', 'builtin', '{"binary_name":"codex","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@zed-industries/codex-acp@0.9.5"]', '[]',
     '[".codex/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'full-access', 3110,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('cc126dd5', '/api/assets/logos/ai-major/gemini.svg', 'Gemini CLI',
     'gemini', 'acp', 'builtin', '{"binary_name":"gemini"}',
     1, 'gemini', '["--experimental-acp"]', '[]',
     '[".gemini/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'yolo', 3120,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('26a946ed', '/api/assets/logos/ai-china/qwen.svg', 'Qwen',
     'qwen', 'acp', 'builtin', '{"binary_name":"qwen"}',
     1, 'qwen', '["--acp"]', '[]',
     '[".qwen/skills"]',
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('8b20fd41', '/api/assets/logos/tools/coding/codebuddy.svg', 'CodeBuddy',
     'codebuddy', 'acp', 'builtin', '{"binary_name":"codebuddy","bridge_binary":"bun"}',
     1, 'bun', '["x","--bun","@tencent-ai/codebuddy-code@2.73.0","--acp"]', '[]',
     '[".codebuddy/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     'bypassPermissions', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('da386544', '/api/assets/logos/brand/droid.svg', 'Droid',
     'droid', 'acp', 'builtin', '{"binary_name":"droid"}',
     1, 'droid', '["exec","--output-format","acp"]', '[]',
     '[".factory/skills"]',
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('600c6601', '/api/assets/logos/tools/goose.svg', 'Goose',
     'goose', 'acp', 'builtin', '{"binary_name":"goose"}',
     1, 'goose', '["acp"]', '[]',
     '[".goose/skills"]',
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('eb895030', '/api/assets/logos/brand/auggie.svg', 'Auggie',
     'auggie', 'acp', 'builtin', '{"binary_name":"auggie"}',
     1, 'auggie', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('e241c49c', '/api/assets/logos/ai-china/kimi.svg', 'Kimi',
     'kimi', 'acp', 'builtin', '{"binary_name":"kimi"}',
     1, 'kimi', '["acp"]', '[]',
     '[".kimi/skills"]',
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('53861a53', '/api/assets/logos/tools/coding/opencode-light.svg', 'OpenCode',
     'opencode', 'acp', 'builtin', '{"binary_name":"opencode"}',
     1, 'opencode', '["acp"]', '[]',
     '[".opencode/skills"]',
     '{"supports_side_question":false}',
     'build', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('3cd9d436', '/api/assets/logos/tools/github.svg', 'Copilot',
     'copilot', 'acp', 'builtin', '{"binary_name":"copilot"}',
     1, 'copilot', '["--acp","--stdio"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('1e4afc51', '/api/assets/logos/tools/coding/qoder.png', 'Qoder',
     'qoder', 'acp', 'builtin', '{"binary_name":"qoder"}',
     1, 'qoder', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('65d0f5b2', '/api/assets/logos/ai-major/mistral.svg', 'Vibe',
     'vibe', 'acp', 'builtin', '{"binary_name":"vibe"}',
     1, 'vibe', '[]', '[]',
     '[".vibe/skills"]',
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('a0dfb1ec', '/api/assets/logos/tools/coding/cursor.png', 'Cursor',
     'cursor', 'acp', 'builtin', '{"binary_name":"cursor"}',
     1, 'cursor', '["acp"]', '[]',
     '[".cursor/skills"]',
     '{"supports_side_question":false}',
     'agent', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('e044000d', NULL, 'Kiro',
     'kiro', 'acp', 'builtin', '{"binary_name":"kiro"}',
     1, 'kiro', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('55f3ed1c', '/api/assets/logos/brand/hermes.svg', 'Hermes',
     'hermes', 'acp', 'builtin', '{"binary_name":"hermes"}',
     1, 'hermes', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('346b0041', '/api/assets/logos/tools/coding/snow.png', 'Snow',
     'snow', 'acp', 'builtin', '{"binary_name":"snow"}',
     1, 'snow', '["--acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     'yolo', 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Non-ACP builtins
    ('fb1083a5', '/api/assets/logos/tools/nanobot.svg', 'Nanobot',
     NULL, 'nanobot', 'builtin', '{"binary_name":"nanobot"}',
     1, 'nanobot', '["--experimental-acp"]', '[]',
     NULL,
     '{}',
     'yolo', 3990,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    ('f9f61666', '/api/assets/logos/tools/openclaw.svg', 'OpenClaw',
     NULL, 'openclaw-gateway', 'builtin', '{"binary_name":"openclaw"}',
     1, 'openclaw', '[]', '[]',
     NULL,
     '{}',
     'yolo', 3900,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000),

    -- Internal
    ('632f31d2', '/api/assets/logos/brand/aion.svg', 'Aion CLI',
     NULL, 'aionrs', 'internal', '{}',
     1, NULL, '[]', '[]',
     '[".aionrs/skills"]',
     '{"supports_team":true}',
     'yolo', 100,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);

------------------------------------------------------------------------
-- Remote Agents & MCP
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS remote_agents (
    id                 TEXT PRIMARY KEY NOT NULL,
    name               TEXT    NOT NULL,
    protocol           TEXT    NOT NULL,
    url                TEXT    NOT NULL,
    auth_type          TEXT    NOT NULL,
    auth_token         TEXT,
    allow_insecure     INTEGER NOT NULL DEFAULT 0,
    avatar             TEXT,
    description        TEXT,
    device_id          TEXT,
    device_public_key  TEXT,
    device_private_key TEXT,
    device_token       TEXT,
    status             TEXT    NOT NULL DEFAULT 'unknown',
    last_connected_at  INTEGER,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_remote_agents_status ON remote_agents(status);

CREATE TABLE IF NOT EXISTS mcp_servers (
    id               TEXT PRIMARY KEY NOT NULL,
    name             TEXT    NOT NULL UNIQUE,
    description      TEXT,
    enabled          INTEGER NOT NULL DEFAULT 0,
    transport_type   TEXT    NOT NULL,
    transport_config TEXT    NOT NULL,
    tools            TEXT,
    status           TEXT    NOT NULL DEFAULT 'disconnected',
    last_connected   INTEGER,
    original_json    TEXT,
    builtin          INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_name ON mcp_servers(name);
CREATE INDEX IF NOT EXISTS idx_mcp_servers_enabled ON mcp_servers(enabled);

CREATE TABLE IF NOT EXISTS oauth_tokens (
    server_url    TEXT PRIMARY KEY NOT NULL,
    access_token  TEXT    NOT NULL,
    refresh_token TEXT,
    token_type    TEXT    NOT NULL DEFAULT 'bearer',
    expires_at    INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

------------------------------------------------------------------------
-- Assistants
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS assistants (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    description             TEXT,
    avatar                  TEXT,
    preset_agent_type       TEXT NOT NULL DEFAULT 'gemini',
    enabled_skills          TEXT,
    custom_skill_names      TEXT,
    disabled_builtin_skills TEXT,
    prompts                 TEXT,
    models                  TEXT,
    name_i18n               TEXT,
    description_i18n        TEXT,
    prompts_i18n            TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_assistants_updated_at ON assistants(updated_at DESC);

CREATE TABLE IF NOT EXISTS assistant_overrides (
    assistant_id      TEXT PRIMARY KEY,
    enabled           INTEGER NOT NULL DEFAULT 1,
    sort_order        INTEGER NOT NULL DEFAULT 0,
    preset_agent_type TEXT,
    last_used_at      INTEGER,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS assistant_plugins (
    id             TEXT PRIMARY KEY NOT NULL,
    type           TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0,
    config         TEXT    NOT NULL,
    status         TEXT,
    last_connected INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS assistant_users (
    id               TEXT PRIMARY KEY NOT NULL,
    platform_user_id TEXT    NOT NULL,
    platform_type    TEXT    NOT NULL,
    display_name     TEXT,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    session_id       TEXT,
    UNIQUE (platform_user_id, platform_type)
);

CREATE TABLE IF NOT EXISTS assistant_sessions (
    id              TEXT PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL,
    agent_type      TEXT    NOT NULL,
    conversation_id TEXT,
    workspace       TEXT,
    chat_id         TEXT,
    created_at      INTEGER NOT NULL,
    last_activity   INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES assistant_users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user_id ON assistant_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user_chat ON assistant_sessions(user_id, chat_id);

CREATE TABLE IF NOT EXISTS assistant_pairing_codes (
    code             TEXT PRIMARY KEY NOT NULL,
    platform_user_id TEXT    NOT NULL,
    platform_type    TEXT    NOT NULL,
    display_name     TEXT,
    requested_at     INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    status           TEXT    NOT NULL DEFAULT 'pending'
                             CHECK (status IN ('pending', 'approved', 'rejected', 'expired'))
);
CREATE INDEX IF NOT EXISTS idx_pairing_codes_status ON assistant_pairing_codes(status);

------------------------------------------------------------------------
-- Teams
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS teams (
    id             TEXT PRIMARY KEY NOT NULL,
    user_id        TEXT    NOT NULL DEFAULT 'system_default_user',
    name           TEXT    NOT NULL,
    workspace      TEXT    NOT NULL DEFAULT '',
    workspace_mode TEXT    NOT NULL DEFAULT 'shared',
    agents         TEXT    NOT NULL DEFAULT '[]',
    lead_agent_id  TEXT,
    session_mode   TEXT,
    agents_version TEXT    NOT NULL DEFAULT '1.0.0',
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_teams_user_id ON teams(user_id);
CREATE INDEX IF NOT EXISTS idx_teams_updated_at ON teams(updated_at);

CREATE TABLE IF NOT EXISTS mailbox (
    id            TEXT    PRIMARY KEY NOT NULL,
    team_id       TEXT    NOT NULL,
    to_agent_id   TEXT    NOT NULL,
    from_agent_id TEXT    NOT NULL,
    type          TEXT    NOT NULL CHECK (type IN ('message', 'idle_notification', 'shutdown_request')),
    content       TEXT    NOT NULL,
    summary       TEXT,
    files         TEXT,
    read          INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_mailbox_team_to_read ON mailbox(team_id, to_agent_id, read);
CREATE INDEX IF NOT EXISTS idx_mailbox_team_id ON mailbox(team_id);

CREATE TABLE IF NOT EXISTS team_tasks (
    id          TEXT    PRIMARY KEY NOT NULL,
    team_id     TEXT    NOT NULL,
    subject     TEXT    NOT NULL,
    description TEXT,
    status      TEXT    NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'in_progress', 'completed', 'deleted')),
    owner       TEXT,
    blocked_by  TEXT    NOT NULL DEFAULT '[]',
    blocks      TEXT    NOT NULL DEFAULT '[]',
    metadata    TEXT,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_team_tasks_team_id ON team_tasks(team_id);

------------------------------------------------------------------------
-- Cron Jobs
------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS cron_jobs (
    id                   TEXT    PRIMARY KEY NOT NULL,
    name                 TEXT    NOT NULL,
    enabled              INTEGER NOT NULL DEFAULT 1,
    schedule_kind        TEXT    NOT NULL CHECK(schedule_kind IN ('at', 'every', 'cron')),
    schedule_value       TEXT    NOT NULL,
    schedule_tz          TEXT,
    schedule_description TEXT,
    payload_message      TEXT    NOT NULL,
    execution_mode       TEXT    NOT NULL DEFAULT 'existing'
                                 CHECK(execution_mode IN ('existing', 'new_conversation')),
    agent_config         TEXT,
    conversation_id      TEXT    NOT NULL,
    conversation_title   TEXT,
    agent_type           TEXT    NOT NULL,
    created_by           TEXT    NOT NULL CHECK(created_by IN ('user', 'agent')),
    skill_content        TEXT,
    description          TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    next_run_at          INTEGER,
    last_run_at          INTEGER,
    last_status          TEXT    CHECK(last_status IN ('ok', 'error', 'skipped', 'missed')),
    last_error           TEXT,
    run_count            INTEGER NOT NULL DEFAULT 0,
    retry_count          INTEGER NOT NULL DEFAULT 0,
    max_retries          INTEGER NOT NULL DEFAULT 3
);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_conversation ON cron_jobs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run_at) WHERE enabled = 1;
CREATE INDEX IF NOT EXISTS idx_cron_jobs_agent_type ON cron_jobs(agent_type);
