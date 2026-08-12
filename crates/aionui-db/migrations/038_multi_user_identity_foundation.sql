-- Phase 1 multi-user identity foundation.

ALTER TABLE users
    ADD COLUMN site_role TEXT NOT NULL DEFAULT 'member'
    CHECK (site_role IN ('admin', 'member'));

ALTER TABLE users
    ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0
    CHECK (must_change_password IN (0, 1));

-- The legacy/bootstrap identity is the initial site administrator.
UPDATE users
SET site_role = 'admin'
WHERE id = 'system_default_user';

CREATE TABLE auth_sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoke_reason TEXT
);

CREATE INDEX idx_auth_sessions_user_active
    ON auth_sessions(user_id, revoked_at, expires_at);

CREATE TABLE admin_audit_log (
    id TEXT PRIMARY KEY,
    occurred_at INTEGER NOT NULL,
    actor_user_id TEXT,
    actor_username TEXT,
    action TEXT NOT NULL,
    target_user_id TEXT,
    target_username TEXT,
    details TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(details))
);

CREATE INDEX idx_admin_audit_log_cursor
    ON admin_audit_log(occurred_at DESC, id DESC);

-- Audit history is append-only, including for direct database clients.
CREATE TRIGGER admin_audit_log_no_update
BEFORE UPDATE ON admin_audit_log
BEGIN
    SELECT RAISE(ABORT, 'admin audit log is append-only');
END;

CREATE TRIGGER admin_audit_log_no_delete
BEFORE DELETE ON admin_audit_log
BEGIN
    SELECT RAISE(ABORT, 'admin audit log is append-only');
END;
