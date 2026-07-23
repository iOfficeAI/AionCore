ALTER TABLE development_policies RENAME TO _development_policies_phase7_legacy;

CREATE TABLE development_policies (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    isolation_mode TEXT NOT NULL DEFAULT 'host' CHECK (isolation_mode IN ('host', 'docker', 'devcontainer')),
    container_image TEXT,
    devcontainer_config_path TEXT,
    container_cpu_millis INTEGER NOT NULL DEFAULT 1000 CHECK (container_cpu_millis BETWEEN 100 AND 64000),
    container_memory_mb INTEGER NOT NULL DEFAULT 2048 CHECK (container_memory_mb BETWEEN 128 AND 262144),
    container_pids_limit INTEGER NOT NULL DEFAULT 256 CHECK (container_pids_limit BETWEEN 16 AND 32768),
    network_mode TEXT NOT NULL DEFAULT 'none' CHECK (network_mode IN ('none', 'bridge')),
    allowed_secret_keys_json TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(allowed_secret_keys_json) AND json_type(allowed_secret_keys_json) = 'array'),
    allowed_commands_json TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(allowed_commands_json) AND json_type(allowed_commands_json) = 'array'),
    protected_paths_json TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(protected_paths_json) AND json_type(protected_paths_json) = 'array'),
    allowed_network_hosts_json TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(allowed_network_hosts_json) AND json_type(allowed_network_hosts_json) = 'array'),
    protected_branches_json TEXT NOT NULL DEFAULT '["main","master"]'
        CHECK (json_valid(protected_branches_json) AND json_type(protected_branches_json) = 'array'),
    dangerous_confirmation_count INTEGER NOT NULL DEFAULT 2 CHECK (dangerous_confirmation_count BETWEEN 1 AND 2),
    max_duration_ms INTEGER NOT NULL DEFAULT 14400000 CHECK (max_duration_ms > 0),
    max_parallel_agents INTEGER NOT NULL DEFAULT 4 CHECK (max_parallel_agents BETWEEN 1 AND 64),
    max_retries INTEGER NOT NULL DEFAULT 3 CHECK (max_retries BETWEEN 0 AND 100),
    max_cost_microunits INTEGER NOT NULL DEFAULT 0 CHECK (max_cost_microunits >= 0),
    max_total_tokens INTEGER NOT NULL DEFAULT 0 CHECK (max_total_tokens >= 0),
    fallback_model TEXT,
    alert_percent INTEGER NOT NULL DEFAULT 80 CHECK (alert_percent BETWEEN 1 AND 100),
    over_limit_action TEXT NOT NULL DEFAULT 'pause'
        CHECK (over_limit_action IN ('notify', 'pause', 'downgrade_model', 'terminate')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(user_id, project_id)
);

INSERT INTO development_policies (
    id, user_id, project_id, isolation_mode, container_image, devcontainer_config_path,
    container_cpu_millis, container_memory_mb, container_pids_limit, network_mode,
    allowed_secret_keys_json, max_duration_ms, max_parallel_agents, max_retries,
    max_cost_microunits, alert_percent, over_limit_action, created_at, updated_at
)
SELECT
    id, user_id, project_id, isolation_mode, container_image, devcontainer_config_path,
    container_cpu_millis, container_memory_mb, container_pids_limit, network_mode,
    allowed_secret_keys_json, max_duration_ms, max_parallel_agents, max_retries,
    max_cost_microunits, alert_percent, over_limit_action, created_at, updated_at
FROM _development_policies_phase7_legacy;

DROP TABLE _development_policies_phase7_legacy;

CREATE INDEX idx_development_policies_owner ON development_policies(user_id, updated_at DESC);

CREATE TABLE development_secrets (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    encrypted_value TEXT NOT NULL,
    key_version TEXT NOT NULL DEFAULT 'application-v1',
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    expires_at INTEGER,
    revoked_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(user_id, project_id, name)
);

CREATE INDEX idx_development_secrets_owner
    ON development_secrets(user_id, project_id, status, updated_at DESC);

CREATE TABLE development_secret_grants (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    secret_id TEXT NOT NULL REFERENCES development_secrets(id) ON DELETE CASCADE,
    scope_type TEXT NOT NULL CHECK (scope_type IN ('project', 'run', 'agent')),
    scope_id TEXT NOT NULL,
    environment_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    expires_at INTEGER,
    revoked_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(user_id, secret_id, scope_type, scope_id, environment_key)
);

CREATE INDEX idx_development_secret_grants_lookup
    ON development_secret_grants(user_id, project_id, secret_id, status, scope_type, scope_id);

CREATE TABLE development_model_prices (
    id TEXT PRIMARY KEY NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_per_million_microunits INTEGER NOT NULL CHECK (input_per_million_microunits >= 0),
    output_per_million_microunits INTEGER NOT NULL CHECK (output_per_million_microunits >= 0),
    cache_read_per_million_microunits INTEGER NOT NULL CHECK (cache_read_per_million_microunits >= 0),
    cache_write_per_million_microunits INTEGER NOT NULL CHECK (cache_write_per_million_microunits >= 0),
    source_id TEXT NOT NULL,
    version TEXT NOT NULL,
    effective_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(provider, model, source_id, version)
);

CREATE INDEX idx_development_model_prices_lookup
    ON development_model_prices(provider, model, effective_at DESC, created_at DESC);

ALTER TABLE development_usage_events ADD COLUMN conversation_id TEXT;
ALTER TABLE development_usage_events ADD COLUMN agent_id TEXT;
ALTER TABLE development_usage_events ADD COLUMN team_id TEXT;
ALTER TABLE development_usage_events ADD COLUMN provider TEXT;
ALTER TABLE development_usage_events ADD COLUMN model TEXT;
ALTER TABLE development_usage_events ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE development_usage_events ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE development_usage_events ADD COLUMN cost_status TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE development_usage_events ADD COLUMN cost_origin TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE development_usage_events ADD COLUMN price_source_id TEXT;
ALTER TABLE development_usage_events ADD COLUMN price_version TEXT;
ALTER TABLE development_usage_events ADD COLUMN price_effective_at INTEGER;

CREATE INDEX idx_development_usage_conversation
    ON development_usage_events(user_id, conversation_id, created_at DESC);
CREATE INDEX idx_development_usage_agent
    ON development_usage_events(user_id, agent_id, created_at DESC);
CREATE INDEX idx_development_usage_team
    ON development_usage_events(user_id, team_id, created_at DESC);
