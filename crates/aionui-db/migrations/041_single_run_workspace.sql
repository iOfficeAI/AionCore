CREATE TABLE development_single_run_workspaces (
    run_id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    baseline_commit TEXT NOT NULL,
    initial_diff_checksum TEXT NOT NULL,
    initial_diff_path TEXT NOT NULL,
    workspace_lease_id TEXT,
    workspace_path TEXT,
    branch TEXT,
    candidate_commit TEXT,
    safe_point TEXT NOT NULL,
    cleanup_status TEXT NOT NULL DEFAULT 'active',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_single_run_workspace_lease
ON development_single_run_workspaces(workspace_lease_id)
WHERE workspace_lease_id IS NOT NULL;
