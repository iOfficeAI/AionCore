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
    max_duration_ms INTEGER NOT NULL DEFAULT 14400000 CHECK (max_duration_ms > 0),
    max_parallel_agents INTEGER NOT NULL DEFAULT 4 CHECK (max_parallel_agents BETWEEN 1 AND 64),
    max_retries INTEGER NOT NULL DEFAULT 3 CHECK (max_retries BETWEEN 0 AND 100),
    max_cost_microunits INTEGER NOT NULL DEFAULT 0 CHECK (max_cost_microunits >= 0),
    alert_percent INTEGER NOT NULL DEFAULT 80 CHECK (alert_percent BETWEEN 1 AND 100),
    over_limit_action TEXT NOT NULL DEFAULT 'pause' CHECK (over_limit_action IN ('notify', 'pause', 'terminate')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(user_id, project_id)
);

CREATE INDEX idx_development_policies_owner ON development_policies(user_id, updated_at DESC);

CREATE TABLE development_usage_events (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES development_runs(id) ON DELETE CASCADE,
    task_id TEXT REFERENCES team_tasks(id) ON DELETE SET NULL,
    usage_type TEXT NOT NULL CHECK (usage_type IN ('agent_turn', 'quality_gate', 'delivery', 'recovery', 'other')),
    source TEXT NOT NULL CHECK (source IN ('provider', 'agent', 'platform', 'operator')),
    confidence TEXT NOT NULL CHECK (confidence IN ('measured', 'reported', 'estimated')),
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cost_microunits INTEGER NOT NULL DEFAULT 0 CHECK (cost_microunits >= 0),
    duration_ms INTEGER NOT NULL DEFAULT 0 CHECK (duration_ms >= 0),
    retry_count INTEGER NOT NULL DEFAULT 0 CHECK (retry_count >= 0),
    metadata_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata_json)),
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_development_usage_owner_project ON development_usage_events(user_id, project_id, created_at DESC);
CREATE INDEX idx_development_usage_run ON development_usage_events(run_id, created_at DESC);

CREATE TRIGGER development_usage_events_no_update
BEFORE UPDATE ON development_usage_events
BEGIN
    SELECT RAISE(ABORT, 'development usage events are append-only');
END;

CREATE TRIGGER development_usage_events_no_delete
BEFORE DELETE ON development_usage_events
BEGIN
    SELECT RAISE(ABORT, 'development usage events are append-only');
END;

CREATE TABLE development_audit_events (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actor_type TEXT NOT NULL CHECK (actor_type IN ('user', 'agent', 'system', 'channel')),
    actor_id TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES development_runs(id) ON DELETE CASCADE,
    task_id TEXT REFERENCES team_tasks(id) ON DELETE SET NULL,
    result TEXT NOT NULL CHECK (result IN ('success', 'denied', 'failed')),
    redacted_payload_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(redacted_payload_json)),
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_development_audit_owner_project ON development_audit_events(user_id, project_id, created_at DESC);
CREATE INDEX idx_development_audit_run ON development_audit_events(run_id, created_at DESC);

CREATE TRIGGER development_audit_events_no_update
BEFORE UPDATE ON development_audit_events
BEGIN
    SELECT RAISE(ABORT, 'development audit events are append-only');
END;

CREATE TRIGGER development_audit_events_no_delete
BEFORE DELETE ON development_audit_events
BEGIN
    SELECT RAISE(ABORT, 'development audit events are append-only');
END;

CREATE TABLE development_alerts (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES development_runs(id) ON DELETE CASCADE,
    alert_type TEXT NOT NULL CHECK (alert_type IN ('budget', 'environment', 'recovery', 'security', 'delivery')),
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'acknowledged', 'resolved')),
    message TEXT NOT NULL,
    dedupe_key TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    resolved_at INTEGER,
    UNIQUE(user_id, dedupe_key)
);

CREATE INDEX idx_development_alerts_owner_project ON development_alerts(user_id, project_id, status, updated_at DESC);
CREATE INDEX idx_development_alerts_run ON development_alerts(run_id, status, updated_at DESC);

CREATE TABLE development_recovery_records (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES development_runs(id) ON DELETE CASCADE,
    recovery_key TEXT NOT NULL,
    finding TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('healthy', 'manual_required', 'resume', 'terminate', 'interrupted')),
    status_before TEXT,
    status_after TEXT,
    details_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(details_json)),
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, recovery_key)
);

CREATE INDEX idx_development_recovery_owner_project
    ON development_recovery_records(user_id, project_id, created_at DESC);
CREATE INDEX idx_development_recovery_run ON development_recovery_records(run_id, created_at DESC);

