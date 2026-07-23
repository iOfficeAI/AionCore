CREATE TABLE development_delivery_tags (
    id TEXT PRIMARY KEY NOT NULL,
    delivery_id TEXT NOT NULL REFERENCES development_deliveries(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    remote_url TEXT,
    status TEXT NOT NULL DEFAULT 'succeeded'
        CHECK (status IN ('pending', 'succeeded', 'failed', 'unknown_remote_state')),
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(delivery_id, name)
);

CREATE INDEX idx_development_delivery_tags_owner
    ON development_delivery_tags(user_id, delivery_id, updated_at DESC);

CREATE TABLE development_deployments (
    id TEXT PRIMARY KEY NOT NULL,
    deployment_key TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES development_runs(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    environment TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending_approval'
        CHECK (status IN (
            'pending_approval', 'approved', 'running', 'succeeded', 'failed',
            'cancelled', 'unknown_remote_state'
        )),
    requested_by TEXT NOT NULL,
    approved_by TEXT,
    approval_run_id TEXT NOT NULL,
    approval_environment TEXT NOT NULL,
    approval_commit_sha TEXT NOT NULL,
    approval_requester TEXT NOT NULL,
    approval_deployment_key TEXT NOT NULL,
    approval_expires_at INTEGER NOT NULL,
    approved_at INTEGER,
    remote_id TEXT,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(user_id, deployment_key)
);

CREATE INDEX idx_development_deployments_run
    ON development_deployments(user_id, run_id, updated_at DESC);
CREATE INDEX idx_development_deployments_status
    ON development_deployments(status, updated_at ASC);
