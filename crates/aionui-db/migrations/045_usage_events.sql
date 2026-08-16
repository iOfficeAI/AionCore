CREATE TABLE IF NOT EXISTS usage_events (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    backend TEXT NOT NULL,
    conversation_source TEXT NOT NULL,
    conversation_name TEXT,
    assistant_id TEXT,
    assistant_name TEXT,
    model_id TEXT,
    turn_id TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    thought_tokens INTEGER NOT NULL DEFAULT 0,
    cached_read_tokens INTEGER NOT NULL DEFAULT 0,
    cached_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_delta REAL NOT NULL DEFAULT 0,
    session_cost_amount REAL,
    cost_currency TEXT,
    event_source TEXT NOT NULL,
    UNIQUE (user_id, conversation_id, fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_usage_events_user_recorded
    ON usage_events (user_id, recorded_at);

CREATE INDEX IF NOT EXISTS idx_usage_events_user_conversation
    ON usage_events (user_id, conversation_id);
