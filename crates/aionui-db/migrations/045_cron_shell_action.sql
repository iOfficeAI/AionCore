-- Native shell-action cron jobs: run a command via the system shell without
-- starting an agent turn. `action` defaults to 'agent' so every pre-existing
-- job keeps its current execution semantics. `payload_message` carries the
-- shell command for shell-action jobs.
ALTER TABLE cron_jobs ADD COLUMN action TEXT NOT NULL DEFAULT 'agent' CHECK(action IN ('agent', 'shell'));
ALTER TABLE cron_jobs ADD COLUMN shell_workspace TEXT;
ALTER TABLE cron_jobs ADD COLUMN shell_timeout_ms INTEGER;
