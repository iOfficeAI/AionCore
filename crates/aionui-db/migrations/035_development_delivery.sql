CREATE TABLE development_deliveries (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL UNIQUE REFERENCES development_runs(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL DEFAULT 'github',
    repository TEXT,
    branch TEXT NOT NULL,
    base_branch TEXT NOT NULL,
    commit_sha TEXT,
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN (
        'draft', 'prepared', 'no_change', 'pushed', 'pr_open', 'ci_pending',
        'ci_failed', 'ci_passed', 'rework_required', 'merge_ready', 'merged', 'failed'
    )),
    push_status TEXT NOT NULL DEFAULT 'pending',
    pr_number INTEGER,
    pr_url TEXT,
    pr_status TEXT NOT NULL DEFAULT 'not_created',
    ci_status TEXT NOT NULL DEFAULT 'not_started',
    review_status TEXT NOT NULL DEFAULT 'pending',
    merge_status TEXT NOT NULL DEFAULT 'blocked',
    report_json TEXT NOT NULL DEFAULT '{}',
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_development_deliveries_owner ON development_deliveries(user_id, updated_at DESC);
CREATE INDEX idx_development_deliveries_project ON development_deliveries(project_id, updated_at DESC);

CREATE TABLE development_ci_checks (
    id TEXT PRIMARY KEY,
    delivery_id TEXT NOT NULL REFERENCES development_deliveries(id) ON DELETE CASCADE,
    provider_check_id TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'in_progress', 'passed', 'failed', 'cancelled', 'skipped')),
    details_url TEXT,
    summary TEXT,
    rework_task_id TEXT,
    started_at INTEGER,
    completed_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(delivery_id, provider_check_id)
);

CREATE INDEX idx_development_ci_checks_delivery ON development_ci_checks(delivery_id, updated_at DESC);
