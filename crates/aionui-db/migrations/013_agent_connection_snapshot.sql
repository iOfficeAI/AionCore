-- Migration 013: persist the latest local-agent connection snapshot
--
-- Stores the most recent availability probe or session-feedback result on
-- `agent_metadata`. These columns are snapshots, not the live runtime truth.

ALTER TABLE agent_metadata ADD COLUMN last_check_status TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_kind TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_error_code TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_error_message TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_guidance TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_latency_ms INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_check_at INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_success_at INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_failure_at INTEGER;

-- Self-repair overrides: user-supplied executable path and extra env vars,
-- layered on top of the seed row at projection time. Stored plaintext, same
-- as the existing `env` column.
ALTER TABLE agent_metadata ADD COLUMN command_override TEXT;
ALTER TABLE agent_metadata ADD COLUMN env_override TEXT;
