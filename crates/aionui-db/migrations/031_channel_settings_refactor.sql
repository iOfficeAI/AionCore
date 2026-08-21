-- Migration 030: channel connection entity + settings scope (4 segments).
--
-- Segments 1-3: channel refactor A1-A3 (connection entity, users/pairing,
-- conversation bindings). Segment 4: settings-dedup B2 (preference scopes).
-- Segment 1 (channel refactor A1):
--
-- Replaces `assistant_plugins` with `channel_connections`, decoupling the
-- connection instance from the platform type (07-16 §5.2 via the 2026-07-27
-- split plan, task A1):
--
--   * `id` becomes a generated, meaning-free connection id (the legacy rows
--     used the platform type itself as the id);
--   * the platform type moves to `plugin_key`;
--   * `PRIMARY KEY (owner_user_id, id)` stays the composite identity that
--     later segments' composite foreign keys will reference;
--   * phase 1 keeps exactly one instance per (owner, plugin_key) via a
--     unique index — multi-instance is a later product decision, at which
--     point that index is dropped.
--
-- The legacy platform-type id remains recoverable as `plugin_key`, which is
-- how segment 2 backfills `channel_users.connection_id`.

CREATE TABLE IF NOT EXISTS channel_connections (
    id             TEXT    NOT NULL,
    owner_user_id  TEXT    NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    plugin_key     TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    enabled        INTEGER NOT NULL DEFAULT 0,
    config         TEXT    NOT NULL,
    status         TEXT,
    last_connected INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (owner_user_id, id)
);

INSERT INTO channel_connections (
    id, owner_user_id, plugin_key, name, enabled, config, status,
    last_connected, created_at, updated_at
)
SELECT
    'conn_' || lower(hex(randomblob(16))),
    owner_user_id,
    id,
    name,
    enabled,
    config,
    status,
    last_connected,
    created_at,
    updated_at
FROM assistant_plugins;

-- Migration-fatal integrity checks (user_scope_rebuild_checks pattern):
-- inserting `ok = 0` violates the CHECK and aborts the migration.
CREATE TEMPORARY TABLE channel_refactor_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

-- Row conservation: every legacy plugin row became exactly one connection.
INSERT INTO channel_refactor_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM channel_connections) = (SELECT COUNT(*) FROM assistant_plugins)
    THEN 1
    ELSE 0
END;

-- Legacy identity preserved: each (owner, legacy id) is now (owner, plugin_key).
INSERT INTO channel_refactor_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1 FROM assistant_plugins p
        WHERE NOT EXISTS (
            SELECT 1 FROM channel_connections c
            WHERE c.owner_user_id = p.owner_user_id AND c.plugin_key = p.id
        )
    )
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_plugins;
DROP TABLE channel_refactor_checks;

-- Phase 1: one connection per (owner, plugin_key). Dropped when multi-instance
-- lands as a product feature.
CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_connections_single_instance
    ON channel_connections(owner_user_id, plugin_key);
CREATE INDEX IF NOT EXISTS idx_channel_connections_owner_created_at
    ON channel_connections(owner_user_id, created_at ASC);

-- ---------------------------------------------------------------------------
-- Segment 2: channel_users + channel_pairing_requests (channel refactor A2).
--
--   * `assistant_users` → `channel_users`: rows attach to their connection
--     (composite FK), the platform column disappears (derived from the
--     connection), `platform_user_id` becomes `external_user_id`, and
--     revocation becomes a soft delete (`status` = active|revoked with
--     `revoked_at`) so authorization history survives for audit.
--     The legacy `session_id` column is dropped (no stable semantics).
--   * `assistant_pairing_codes` → `channel_pairing_requests`: pairing rows
--     get a surrogate id, attach to their connection, and store only a
--     server-side HMAC of the code (`code_hash`) — the plaintext code never
--     touches the database. Legacy rows are NOT migrated: pairing codes are
--     10-minute artifacts whose plaintext cannot (and must not) be hashed
--     retroactively, and historical rows carry no runtime state.
--   * `conversations` gains UNIQUE(user_id, id) — the shared foundation for
--     composite cross-account foreign keys (07-16 §5.4), used by segment 3.
--
-- Legacy assistant_users rows whose platform has no connection row (the
-- plugin row was deleted after users were authorized) get a synthesized
-- disabled connection so no authorization silently disappears.
-- ---------------------------------------------------------------------------

INSERT INTO channel_connections (
    id, owner_user_id, plugin_key, name, enabled, config, created_at, updated_at
)
SELECT
    'conn_' || lower(hex(randomblob(16))),
    u.owner_user_id,
    u.platform_type,
    u.platform_type || ' (recovered)',
    0,
    '',
    strftime('%s', 'now') * 1000,
    strftime('%s', 'now') * 1000
FROM (
    SELECT DISTINCT owner_user_id, platform_type FROM assistant_users
) u
WHERE NOT EXISTS (
    SELECT 1 FROM channel_connections c
    WHERE c.owner_user_id = u.owner_user_id AND c.plugin_key = u.platform_type
);

CREATE TABLE channel_users (
    id               TEXT    PRIMARY KEY NOT NULL,
    owner_user_id    TEXT    NOT NULL REFERENCES users(id),
    connection_id    TEXT    NOT NULL,
    external_user_id TEXT    NOT NULL,
    display_name     TEXT,
    status           TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    revoked_at       INTEGER,
    authorized_at    INTEGER NOT NULL,
    last_active      INTEGER,
    FOREIGN KEY (owner_user_id, connection_id) REFERENCES channel_connections(owner_user_id, id),
    UNIQUE (owner_user_id, connection_id, external_user_id)
);

INSERT INTO channel_users (
    id, owner_user_id, connection_id, external_user_id, display_name,
    status, revoked_at, authorized_at, last_active
)
SELECT
    u.id, u.owner_user_id, c.id, u.platform_user_id, u.display_name,
    'active', NULL, u.authorized_at, u.last_active
FROM assistant_users u
JOIN channel_connections c
    ON c.owner_user_id = u.owner_user_id AND c.plugin_key = u.platform_type;

CREATE TEMPORARY TABLE channel_refactor_checks_2 (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

-- Row conservation: every authorized user survived the rebuild.
INSERT INTO channel_refactor_checks_2 (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM channel_users) = (SELECT COUNT(*) FROM assistant_users)
    THEN 1
    ELSE 0
END;

-- Rebuild assistant_sessions so its user FK follows the renamed parent.
-- Shape is unchanged here; segment 3 reshapes it into
-- channel_conversation_bindings.
CREATE TABLE assistant_sessions_new (
    id              TEXT PRIMARY KEY NOT NULL,
    user_id         TEXT    NOT NULL REFERENCES channel_users(id) ON DELETE CASCADE,
    agent_type      TEXT    NOT NULL,
    conversation_id TEXT,
    workspace       TEXT,
    chat_id         TEXT,
    created_at      INTEGER NOT NULL,
    last_activity   INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);

INSERT INTO assistant_sessions_new SELECT * FROM assistant_sessions;

INSERT INTO channel_refactor_checks_2 (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM assistant_sessions_new) = (SELECT COUNT(*) FROM assistant_sessions)
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_sessions;
ALTER TABLE assistant_sessions_new RENAME TO assistant_sessions;
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_user ON assistant_sessions(user_id);
CREATE INDEX IF NOT EXISTS idx_assistant_sessions_conversation ON assistant_sessions(conversation_id);

DROP TABLE assistant_users;
DROP TABLE channel_refactor_checks_2;

-- Pairing requests: hashed codes only; legacy 10-minute codes are dropped by
-- design (see segment header).
CREATE TABLE channel_pairing_requests (
    id                       TEXT    PRIMARY KEY NOT NULL,
    owner_user_id            TEXT    NOT NULL REFERENCES users(id),
    connection_id            TEXT    NOT NULL,
    external_user_id         TEXT    NOT NULL,
    display_name             TEXT,
    code_hash                TEXT    NOT NULL,
    status                   TEXT    NOT NULL DEFAULT 'pending'
                                     CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    requested_at             INTEGER NOT NULL,
    expires_at               INTEGER NOT NULL,
    approved_channel_user_id TEXT    REFERENCES channel_users(id),
    FOREIGN KEY (owner_user_id, connection_id) REFERENCES channel_connections(owner_user_id, id)
);

DROP TABLE assistant_pairing_codes;

-- One pending request per (owner, connection, external user); one pending
-- request per (owner, code hash).
CREATE UNIQUE INDEX idx_channel_pairing_pending_user
    ON channel_pairing_requests(owner_user_id, connection_id, external_user_id)
    WHERE status = 'pending';
CREATE UNIQUE INDEX idx_channel_pairing_pending_hash
    ON channel_pairing_requests(owner_user_id, code_hash)
    WHERE status = 'pending';
CREATE INDEX idx_channel_pairing_owner_status_expiry
    ON channel_pairing_requests(owner_user_id, status, expires_at);

-- Shared foundation for composite cross-account FKs (07-16 §5.4).
CREATE UNIQUE INDEX IF NOT EXISTS idx_conversations_user_id_id
    ON conversations(user_id, id);

-- ---------------------------------------------------------------------------
-- Segment 3: assistant_sessions → channel_conversation_bindings (A3).
--
--   * The binding attaches to its connection and channel user directly
--     (owner_user_id + connection_id + channel_user_id columns, composite FK
--     into channel_users), instead of deriving the owner through a join.
--   * `agent_type` and `workspace` are dropped: agent configuration is owned
--     by channel settings + the conversation snapshot, and the workspace
--     column never had a production reader.
--   * `chat_id` becomes `external_chat_id` (nullable is preserved: legacy
--     rows without a chat id keep their history; new sessions always carry
--     one). `last_activity` becomes `last_active_at`.
--   * Uniqueness: one binding per (owner, connection, channel user, external
--     chat) — "same user, different chat, different context" stays.
--   * Cross-account guard against conversations: enforced by triggers rather
--     than a composite FK — a composite FK's ON DELETE SET NULL would null
--     owner_user_id together with conversation_id, and NO ACTION would block
--     conversation deletion. The single-column conversation FK keeps its
--     ON DELETE SET NULL semantics; the triggers make a cross-account
--     binding unrepresentable (07-16 §5.4 intent).
-- ---------------------------------------------------------------------------

-- Composite FK target for (owner, connection, channel user).
CREATE UNIQUE INDEX IF NOT EXISTS idx_channel_users_owner_connection_id
    ON channel_users(owner_user_id, connection_id, id);

CREATE TABLE channel_conversation_bindings (
    id               TEXT    PRIMARY KEY NOT NULL,
    owner_user_id    TEXT    NOT NULL REFERENCES users(id),
    connection_id    TEXT    NOT NULL,
    channel_user_id  TEXT    NOT NULL,
    external_chat_id TEXT,
    conversation_id  TEXT    REFERENCES conversations(id) ON DELETE SET NULL,
    created_at       INTEGER NOT NULL,
    last_active_at   INTEGER NOT NULL,
    FOREIGN KEY (owner_user_id, connection_id, channel_user_id)
        REFERENCES channel_users(owner_user_id, connection_id, id) ON DELETE CASCADE,
    UNIQUE (owner_user_id, connection_id, channel_user_id, external_chat_id)
);

INSERT INTO channel_conversation_bindings (
    id, owner_user_id, connection_id, channel_user_id, external_chat_id,
    conversation_id, created_at, last_active_at
)
SELECT
    s.id, u.owner_user_id, u.connection_id, s.user_id, s.chat_id,
    s.conversation_id, s.created_at, s.last_activity
FROM assistant_sessions s
JOIN channel_users u ON u.id = s.user_id;

CREATE TEMPORARY TABLE channel_refactor_checks_3 (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

-- Row conservation: every session became exactly one binding.
INSERT INTO channel_refactor_checks_3 (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM channel_conversation_bindings) = (SELECT COUNT(*) FROM assistant_sessions)
    THEN 1
    ELSE 0
END;

-- No binding may reference a conversation owned by another Core user.
INSERT INTO channel_refactor_checks_3 (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1 FROM channel_conversation_bindings b
        JOIN conversations c ON c.id = b.conversation_id
        WHERE c.user_id != b.owner_user_id
    )
    THEN 1
    ELSE 0
END;

DROP TABLE assistant_sessions;
DROP TABLE channel_refactor_checks_3;

CREATE INDEX IF NOT EXISTS idx_channel_bindings_owner_last_active
    ON channel_conversation_bindings(owner_user_id, last_active_at DESC);
CREATE INDEX IF NOT EXISTS idx_channel_bindings_conversation
    ON channel_conversation_bindings(conversation_id);

-- Cross-account guard (see segment header): binding a conversation owned by
-- a different Core user is unrepresentable.
CREATE TRIGGER trg_channel_binding_conversation_owner_insert
BEFORE INSERT ON channel_conversation_bindings
FOR EACH ROW
WHEN NEW.conversation_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM conversations c
    WHERE c.id = NEW.conversation_id AND c.user_id = NEW.owner_user_id
)
BEGIN
    SELECT RAISE(ABORT, 'CROSS_ACCOUNT_REFERENCE: conversation belongs to another user');
END;

CREATE TRIGGER trg_channel_binding_conversation_owner_update
BEFORE UPDATE OF conversation_id, owner_user_id ON channel_conversation_bindings
FOR EACH ROW
WHEN NEW.conversation_id IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM conversations c
    WHERE c.id = NEW.conversation_id AND c.user_id = NEW.owner_user_id
)
BEGIN
    SELECT RAISE(ABORT, 'CROSS_ACCOUNT_REFERENCE: conversation belongs to another user');
END;

-- ---------------------------------------------------------------------------
-- Segment 4: device/account scope for client_preferences (settings-dedup B2;
-- disposition table in docs/superpowers/2026-07-27-settings-dedup-b1-inventory.md).
-- Folded into this migration so the combined channel/settings refactor ships
-- as a single migration file.
-- ---------------------------------------------------------------------------

CREATE TABLE client_preferences_new (
    scope      TEXT    NOT NULL DEFAULT 'account' CHECK (scope IN ('device', 'account')),
    user_id    TEXT    REFERENCES users(id),
    key        TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (
        (scope = 'device' AND user_id IS NULL)
        OR (scope = 'account' AND user_id IS NOT NULL)
    )
);

INSERT INTO client_preferences_new (scope, user_id, key, value, updated_at)
SELECT 'account', user_id, key, value, updated_at
FROM client_preferences;

CREATE TEMPORARY TABLE client_preference_scope_checks (
    ok INTEGER NOT NULL CHECK (ok = 1)
);

INSERT INTO client_preference_scope_checks (ok)
SELECT CASE
    WHEN (SELECT COUNT(*) FROM client_preferences_new) = (SELECT COUNT(*) FROM client_preferences)
    THEN 1
    ELSE 0
END;

DROP TABLE client_preferences;
ALTER TABLE client_preferences_new RENAME TO client_preferences;

-- Scope-aware uniqueness: one device value per key, one account value per
-- (user, key).
CREATE UNIQUE INDEX idx_client_preferences_device_key
    ON client_preferences(key) WHERE scope = 'device';
CREATE UNIQUE INDEX idx_client_preferences_account_key
    ON client_preferences(user_id, key) WHERE scope = 'account';

-- Promote confirmed device-level keys: latest write wins across users.
INSERT INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'device', NULL, key, value, updated_at
FROM (
    SELECT
        key, value, updated_at,
        ROW_NUMBER() OVER (PARTITION BY key ORDER BY updated_at DESC, user_id ASC) AS rn
    FROM client_preferences
    WHERE scope = 'account'
      AND (
          key IN ('system.closeToTray', 'keepAwake', 'autoPreviewOfficeFiles')
          OR key LIKE 'pet.%'
      )
)
WHERE rn = 1;

DELETE FROM client_preferences
WHERE scope = 'account'
  AND (
      key IN ('system.closeToTray', 'keepAwake', 'autoPreviewOfficeFiles')
      OR key LIKE 'pet.%'
  );

-- Materialize the system_settings switches as account-scope keys. INSERT OR
-- IGNORE: an existing preference row (e.g. written post-B1) is the newer
-- truth and must not be clobbered.
INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.notificationEnabled',
       CASE WHEN s.notification_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'cron.notificationEnabled',
       CASE WHEN s.cron_notification_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.commandQueueEnabled',
       CASE WHEN s.command_queue_enabled THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

INSERT OR IGNORE INTO client_preferences (scope, user_id, key, value, updated_at)
SELECT 'account', s.user_id, 'system.saveUploadToWorkspace',
       CASE WHEN s.save_upload_to_workspace THEN 'true' ELSE 'false' END, s.updated_at
FROM system_settings s;

-- Migration validation: every migrated switch column is readable back as a
-- preference for every settings row.
INSERT INTO client_preference_scope_checks (ok)
SELECT CASE
    WHEN NOT EXISTS (
        SELECT 1 FROM system_settings s
        WHERE NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.notificationEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'cron.notificationEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.commandQueueEnabled'
        )
        OR NOT EXISTS (
            SELECT 1 FROM client_preferences p
            WHERE p.scope = 'account' AND p.user_id = s.user_id
              AND p.key = 'system.saveUploadToWorkspace'
        )
    )
    THEN 1
    ELSE 0
END;

DROP TABLE client_preference_scope_checks;
