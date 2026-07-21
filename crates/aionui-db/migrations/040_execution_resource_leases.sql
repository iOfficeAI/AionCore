CREATE TABLE execution_resource_leases (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT NOT NULL REFERENCES development_runs(id) ON DELETE CASCADE,
    task_id TEXT,
    turn_id TEXT,
    gate_id TEXT,
    environment_id TEXT NOT NULL,
    environment_kind TEXT NOT NULL CHECK (environment_kind IN ('host', 'docker', 'devcontainer')),
    resource_kind TEXT NOT NULL CHECK (resource_kind IN ('process', 'container', 'service', 'port', 'lock', 'workspace')),
    resource_identifier TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'stopping', 'released', 'cleanup_failed', 'orphaned')),
    accepts_work INTEGER NOT NULL DEFAULT 1 CHECK (accepts_work IN (0, 1)),
    owner_instance_id TEXT NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    cleanup_order INTEGER NOT NULL,
    cleanup_status TEXT,
    cleanup_result TEXT,
    recovery_decision TEXT CHECK (recovery_decision IS NULL OR recovery_decision IN ('retry', 'rollback', 'takeover', 'terminate')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    terminal_at INTEGER
);

CREATE INDEX idx_execution_resource_leases_run
    ON execution_resource_leases(user_id, run_id, status, cleanup_order);

CREATE INDEX idx_execution_resource_leases_heartbeat
    ON execution_resource_leases(status, accepts_work, expires_at, heartbeat_at);

CREATE TABLE execution_environment_bindings (
    entity_type TEXT NOT NULL CHECK (entity_type IN ('run', 'task', 'turn', 'gate')),
    entity_id TEXT NOT NULL,
    environment_id TEXT NOT NULL,
    environment_kind TEXT NOT NULL CHECK (environment_kind IN ('host', 'docker', 'devcontainer')),
    bound_at INTEGER NOT NULL,
    PRIMARY KEY (entity_type, entity_id, environment_id)
);

CREATE INDEX idx_execution_environment_bindings_environment
    ON execution_environment_bindings(environment_id, entity_type, entity_id);
