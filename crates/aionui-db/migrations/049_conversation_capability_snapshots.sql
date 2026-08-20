CREATE TABLE IF NOT EXISTS conversation_capability_snapshots (
    conversation_id  TEXT PRIMARY KEY,
    user_id          TEXT NOT NULL,
    revision         INTEGER NOT NULL,
    capabilities_json TEXT NOT NULL,
    backend_identity TEXT NOT NULL,
    negotiated_at    INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_conversation_capability_snapshots_user
    ON conversation_capability_snapshots(user_id, updated_at DESC);
