CREATE TABLE IF NOT EXISTS conversation_inputs (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    mode            TEXT NOT NULL CHECK (mode IN ('followup', 'steer', 'inject')),
    status          TEXT NOT NULL CHECK (status IN ('held', 'dispatching', 'accepted', 'applied', 'canceled', 'failed')),
    content         TEXT NOT NULL,
    files           TEXT NOT NULL DEFAULT '[]',
    inject_skills   TEXT NOT NULL DEFAULT '[]',
    hidden          INTEGER NOT NULL DEFAULT 0,
    client_key      TEXT NOT NULL,
    turn_id         TEXT,
    msg_id          TEXT,
    error_code      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    UNIQUE (user_id, conversation_id, client_key)
);

CREATE INDEX IF NOT EXISTS idx_conversation_inputs_queue
    ON conversation_inputs(user_id, conversation_id, status, created_at, id);

UPDATE system_settings SET command_queue_enabled = 1;
