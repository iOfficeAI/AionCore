ALTER TABLE cron_jobs ADD COLUMN queue_enabled INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS cron_job_runs (
    id              TEXT NOT NULL PRIMARY KEY,
    job_id          TEXT NOT NULL,
    scheduled_at    INTEGER NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('running', 'retrying', 'ok', 'error', 'skipped')),
    owner_id        TEXT,
    lease_until     INTEGER,
    conversation_id TEXT,
    error           TEXT,
    started_at      INTEGER,
    finished_at     INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE(job_id, scheduled_at),
    FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_cron_job_runs_active
    ON cron_job_runs(job_id, lease_until)
    WHERE status IN ('running', 'retrying');
