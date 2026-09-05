-- Agent Center MVP: side tables on top of assistant_definitions.
-- Does NOT alter assistant_definitions columns (preserve existing CRUD / SELECT *).
-- visibility/status defaults match product: private draft; empty MCP allowlist = mount none.

CREATE TABLE IF NOT EXISTS assistant_agent_center (
    assistant_definition_id TEXT PRIMARY KEY NOT NULL
        REFERENCES assistant_definitions(id) ON DELETE CASCADE,
    visibility TEXT NOT NULL DEFAULT 'private'
        CHECK (visibility IN ('private', 'team', 'enterprise')),
    team_id TEXT,
    enterprise_id TEXT,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'published', 'archived')),
    version INTEGER NOT NULL DEFAULT 0,
    published_revision_id TEXT,
    -- JSON array of KnowHub scope refs (optional; may be empty while KnowHub builds)
    knowledge_scopes TEXT NOT NULL DEFAULT '[]',
    -- JSON array of {skill_key, source?, version_policy, pinned_version?}
    skill_refs TEXT NOT NULL DEFAULT '[]',
    -- allowlist (default): only default_mcp_ids; empty list = mount no MCP
    -- inherit_user_enabled: explicit opt-in to user global enabled MCP set
    mcp_policy TEXT NOT NULL DEFAULT 'allowlist'
        CHECK (mcp_policy IN ('allowlist', 'inherit_user_enabled')),
    -- JSON array of {subject_type, subject_id, role}
    role_bindings TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_assistant_agent_center_visibility
    ON assistant_agent_center(visibility);
CREATE INDEX IF NOT EXISTS idx_assistant_agent_center_team
    ON assistant_agent_center(team_id);
CREATE INDEX IF NOT EXISTS idx_assistant_agent_center_status
    ON assistant_agent_center(status);

CREATE TABLE IF NOT EXISTS assistant_definition_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    assistant_definition_id TEXT NOT NULL
        REFERENCES assistant_definitions(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    snapshot_json TEXT NOT NULL,
    changelog TEXT,
    created_by TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (assistant_definition_id, revision)
);

CREATE INDEX IF NOT EXISTS idx_assistant_definition_revisions_def
    ON assistant_definition_revisions(assistant_definition_id, revision DESC);
