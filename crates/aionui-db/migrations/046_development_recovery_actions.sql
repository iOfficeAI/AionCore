CREATE TABLE development_recovery_records_v2 (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    run_id TEXT REFERENCES development_runs(id) ON DELETE CASCADE,
    recovery_key TEXT NOT NULL,
    finding TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (
        decision IN (
            'healthy',
            'manual_required',
            'resume',
            'retry',
            'rollback',
            'takeover',
            'terminate',
            'interrupted'
        )
    ),
    status_before TEXT,
    status_after TEXT,
    details_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(details_json)),
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, recovery_key)
);

INSERT INTO development_recovery_records_v2 (
    id,
    user_id,
    project_id,
    run_id,
    recovery_key,
    finding,
    decision,
    status_before,
    status_after,
    details_json,
    created_at
)
SELECT
    id,
    user_id,
    project_id,
    run_id,
    recovery_key,
    finding,
    decision,
    status_before,
    status_after,
    details_json,
    created_at
FROM development_recovery_records;

DROP TABLE development_recovery_records;
ALTER TABLE development_recovery_records_v2 RENAME TO development_recovery_records;

CREATE INDEX idx_development_recovery_owner_project
    ON development_recovery_records(user_id, project_id, created_at DESC);
CREATE INDEX idx_development_recovery_run
    ON development_recovery_records(run_id, created_at DESC);
