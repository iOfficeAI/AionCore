DROP INDEX IF EXISTS idx_team_tasks_team_id;

ALTER TABLE team_tasks RENAME TO _team_tasks_phase3_legacy;

CREATE TABLE team_tasks (
    id                          TEXT PRIMARY KEY NOT NULL,
    team_id                     TEXT NOT NULL,
    run_id                      TEXT,
    subject                     TEXT NOT NULL,
    description                 TEXT,
    status                      TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN (
                                    'pending', 'ready', 'claimed', 'in_progress', 'waiting_approval',
                                    'verifying', 'review', 'rework', 'completed', 'failed', 'cancelled', 'deleted'
                                )),
    owner                       TEXT,
    blocked_by                  TEXT NOT NULL DEFAULT '[]',
    blocks                      TEXT NOT NULL DEFAULT '[]',
    metadata                    TEXT,
    acceptance_criteria         TEXT NOT NULL DEFAULT '[]',
    task_type                   TEXT NOT NULL DEFAULT 'implementation',
    risk_level                  TEXT NOT NULL DEFAULT 'medium'
                                CHECK (risk_level IN ('low', 'medium', 'high', 'critical')),
    assigned_workspace_lease_id TEXT,
    review_status               TEXT NOT NULL DEFAULT 'pending'
                                CHECK (review_status IN ('pending', 'in_review', 'approved', 'changes_requested', 'not_required')),
    verification_status         TEXT NOT NULL DEFAULT 'pending'
                                CHECK (verification_status IN ('pending', 'running', 'passed', 'failed', 'not_required')),
    created_at                  INTEGER NOT NULL,
    updated_at                  INTEGER NOT NULL
);

INSERT INTO team_tasks (
    id, team_id, subject, description, status, owner, blocked_by, blocks, metadata, created_at, updated_at
)
SELECT id, team_id, subject, description, status, owner, blocked_by, blocks, metadata, created_at, updated_at
FROM _team_tasks_phase3_legacy;

DROP TABLE _team_tasks_phase3_legacy;

CREATE INDEX idx_team_tasks_team_id ON team_tasks(team_id);
CREATE INDEX idx_team_tasks_run_id ON team_tasks(run_id, status);
CREATE INDEX idx_team_tasks_workspace_lease ON team_tasks(assigned_workspace_lease_id);

CREATE TABLE development_runs (
    id                  TEXT PRIMARY KEY NOT NULL,
    user_id             TEXT NOT NULL,
    project_id          TEXT NOT NULL,
    team_id             TEXT,
    source_channel      TEXT,
    source_user_id      TEXT,
    execution_mode      TEXT NOT NULL CHECK (execution_mode IN ('single', 'team')),
    status              TEXT NOT NULL DEFAULT 'draft'
                        CHECK (status IN (
                            'draft', 'preflight', 'running', 'waiting_approval', 'verifying', 'reviewing',
                            'integrating', 'rework', 'paused', 'succeeded', 'failed', 'cancelled'
                        )),
    request_summary     TEXT NOT NULL,
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    baseline_commit     TEXT,
    integration_branch  TEXT,
    started_at          INTEGER,
    finished_at         INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE INDEX idx_development_runs_user_project ON development_runs(user_id, project_id, updated_at DESC);
CREATE INDEX idx_development_runs_team ON development_runs(team_id, updated_at DESC);

CREATE TABLE task_artifacts (
    id                TEXT PRIMARY KEY NOT NULL,
    run_id            TEXT NOT NULL,
    task_id           TEXT,
    artifact_type     TEXT NOT NULL CHECK (artifact_type IN ('diff', 'test', 'log', 'report', 'commit', 'review', 'no_code_change')),
    path_or_uri       TEXT NOT NULL,
    checksum          TEXT NOT NULL,
    producer_agent_id TEXT,
    metadata          TEXT,
    created_at        INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_task_artifacts_task ON task_artifacts(task_id, created_at ASC);
CREATE INDEX idx_task_artifacts_run ON task_artifacts(run_id, created_at ASC);

CREATE TABLE quality_gate_runs (
    id                 TEXT PRIMARY KEY NOT NULL,
    run_id             TEXT NOT NULL,
    task_id            TEXT,
    gate_type          TEXT NOT NULL,
    command            TEXT NOT NULL,
    working_directory  TEXT NOT NULL,
    exit_code           INTEGER,
    status              TEXT NOT NULL CHECK (status IN ('queued', 'running', 'passed', 'failed', 'timed_out', 'cancelled')),
    stdout_artifact_id  TEXT,
    stderr_artifact_id  TEXT,
    duration_ms         INTEGER,
    required            INTEGER NOT NULL DEFAULT 1,
    started_at          INTEGER,
    finished_at         INTEGER,
    created_at          INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE CASCADE,
    FOREIGN KEY(stdout_artifact_id) REFERENCES task_artifacts(id) ON DELETE SET NULL,
    FOREIGN KEY(stderr_artifact_id) REFERENCES task_artifacts(id) ON DELETE SET NULL
);

CREATE INDEX idx_quality_gate_runs_task ON quality_gate_runs(task_id, gate_type, created_at DESC);
CREATE INDEX idx_quality_gate_runs_run ON quality_gate_runs(run_id, created_at DESC);

CREATE TABLE review_findings (
    id                TEXT PRIMARY KEY NOT NULL,
    run_id            TEXT NOT NULL,
    task_id           TEXT NOT NULL,
    reviewer_agent_id TEXT NOT NULL,
    producer_agent_id TEXT,
    severity          TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'major', 'critical', 'blocker')),
    file_path         TEXT,
    line_number       INTEGER,
    reason            TEXT NOT NULL,
    suggestion        TEXT,
    status            TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'resolved', 'dismissed')),
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE CASCADE,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE CASCADE
);

CREATE INDEX idx_review_findings_task ON review_findings(task_id, status, severity);
