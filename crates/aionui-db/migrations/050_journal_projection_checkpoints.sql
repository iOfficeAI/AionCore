CREATE TABLE IF NOT EXISTS journal_projection_checkpoints (
    user_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    projector TEXT NOT NULL,
    last_sequence INTEGER NOT NULL,
    last_event_id TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, conversation_id, projector),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_journal_projection_checkpoints_conversation
    ON journal_projection_checkpoints(conversation_id, projector);
