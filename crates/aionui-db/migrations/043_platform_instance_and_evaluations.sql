CREATE TABLE platform_instances (
    singleton INTEGER PRIMARY KEY NOT NULL DEFAULT 1 CHECK (singleton = 1),
    instance_id TEXT NOT NULL UNIQUE,
    schema_version INTEGER NOT NULL,
    app_version TEXT NOT NULL,
    first_started_at INTEGER NOT NULL,
    last_started_at INTEGER NOT NULL
);

INSERT INTO platform_instances (
    singleton, instance_id, schema_version, app_version, first_started_at, last_started_at
) VALUES (
    1, lower(hex(randomblob(16))), 34, '0.1.39',
    CAST(unixepoch('subsec') * 1000 AS INTEGER),
    CAST(unixepoch('subsec') * 1000 AS INTEGER)
);

CREATE TABLE development_evaluations (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    release_id TEXT NOT NULL,
    scenario_id TEXT NOT NULL,
    result TEXT NOT NULL CHECK (result IN ('passed', 'failed', 'error', 'skipped')),
    duration_ms INTEGER NOT NULL CHECK (duration_ms >= 0),
    failure_category TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cost_microunits INTEGER NOT NULL DEFAULT 0 CHECK (cost_microunits >= 0),
    cost_source TEXT NOT NULL,
    accepted_baseline INTEGER NOT NULL DEFAULT 0 CHECK (accepted_baseline IN (0, 1)),
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, project_id, release_id, scenario_id)
);

CREATE INDEX idx_development_evaluations_release
    ON development_evaluations(user_id, project_id, release_id, scenario_id);

CREATE INDEX idx_development_evaluations_baseline
    ON development_evaluations(user_id, project_id, scenario_id, accepted_baseline, created_at DESC);

CREATE TABLE development_evaluation_baselines (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    release_id TEXT NOT NULL,
    accepted_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, project_id)
);

CREATE TABLE development_retention_policies (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    conversation_history_days INTEGER NOT NULL DEFAULT 365 CHECK (conversation_history_days BETWEEN 1 AND 3650),
    artifact_days INTEGER NOT NULL DEFAULT 90 CHECK (artifact_days BETWEEN 1 AND 3650),
    evaluation_days INTEGER NOT NULL DEFAULT 365 CHECK (evaluation_days BETWEEN 1 AND 3650),
    immutable_audit_log INTEGER NOT NULL DEFAULT 1 CHECK (immutable_audit_log = 1),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, project_id)
);

CREATE TABLE development_retention_executions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    message_count INTEGER NOT NULL CHECK (message_count >= 0),
    artifact_count INTEGER NOT NULL CHECK (artifact_count >= 0),
    evaluation_count INTEGER NOT NULL CHECK (evaluation_count >= 0),
    audit_events_retained INTEGER NOT NULL CHECK (audit_events_retained >= 0),
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_development_retention_executions_project
    ON development_retention_executions(user_id, project_id, created_at DESC);
