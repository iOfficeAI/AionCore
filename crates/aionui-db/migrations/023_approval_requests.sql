CREATE TABLE approval_requests (
    id                 TEXT PRIMARY KEY NOT NULL,
    requester_user_id  TEXT NOT NULL,
    project_id         TEXT,
    run_id             TEXT,
    task_id            TEXT,
    conversation_id    TEXT NOT NULL,
    agent_id            TEXT,
    call_id             TEXT NOT NULL,
    action_type         TEXT NOT NULL,
    command             TEXT,
    working_directory   TEXT,
    risk_level          TEXT NOT NULL DEFAULT 'medium'
                        CHECK (risk_level IN ('low', 'medium', 'high', 'critical')),
    options             TEXT NOT NULL DEFAULT '[]',
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'approved', 'rejected', 'expired', 'cancelled')),
    approver_user_id    TEXT,
    source_channel      TEXT,
    source_chat_id      TEXT,
    source_thread_id    INTEGER,
    expires_at          INTEGER NOT NULL,
    consumed_at         INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE(conversation_id, call_id),
    FOREIGN KEY(requester_user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY(project_id) REFERENCES projects(id) ON DELETE SET NULL,
    FOREIGN KEY(run_id) REFERENCES development_runs(id) ON DELETE SET NULL,
    FOREIGN KEY(task_id) REFERENCES team_tasks(id) ON DELETE SET NULL,
    FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY(approver_user_id) REFERENCES users(id) ON DELETE SET NULL
);

CREATE INDEX idx_approval_requests_user_status
    ON approval_requests(requester_user_id, status, created_at DESC);
CREATE INDEX idx_approval_requests_expiry
    ON approval_requests(status, expires_at);
CREATE INDEX idx_approval_requests_run_status
    ON approval_requests(run_id, status, created_at DESC);
