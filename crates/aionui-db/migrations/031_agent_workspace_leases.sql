CREATE TABLE IF NOT EXISTS agent_workspace_leases (
    id TEXT PRIMARY KEY NOT NULL,
    team_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    slot_id TEXT NOT NULL,
    workspace_mode TEXT NOT NULL,
    repository_path TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    branch_name TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    allowed_paths TEXT NOT NULL DEFAULT '["."]',
    lease_status TEXT NOT NULL DEFAULT 'provisioning',
    cleanup_status TEXT NOT NULL DEFAULT 'none',
    conflict_files TEXT NOT NULL DEFAULT '[]',
    last_error TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    released_at INTEGER,
    UNIQUE(team_id, slot_id),
    UNIQUE(worktree_path),
    UNIQUE(repository_path, branch_name)
);

CREATE INDEX IF NOT EXISTS idx_workspace_leases_team
    ON agent_workspace_leases(team_id, slot_id);

CREATE INDEX IF NOT EXISTS idx_workspace_leases_reconcile
    ON agent_workspace_leases(lease_status, updated_at);
