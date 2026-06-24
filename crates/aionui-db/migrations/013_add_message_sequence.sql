-- Migration 013: Add per-conversation message sequence for cursor pagination

CREATE TABLE IF NOT EXISTS _messages_new (
    id              TEXT    PRIMARY KEY NOT NULL,
    conversation_id TEXT    NOT NULL,
    msg_id          TEXT,
    type            TEXT    NOT NULL,
    content         TEXT    NOT NULL DEFAULT '{}',
    position        TEXT    CHECK(position IN ('left', 'right', 'center', 'pop')),
    status          TEXT    CHECK(status IN ('finish', 'pending', 'error', 'work')),
    hidden          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    sequence        INTEGER NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

INSERT OR IGNORE INTO _messages_new
    (id, conversation_id, msg_id, type, content, position, status, hidden, created_at, sequence)
SELECT
    id,
    conversation_id,
    msg_id,
    type,
    COALESCE(content, '{}'),
    position,
    status,
    COALESCE(hidden, 0),
    created_at,
    ROW_NUMBER() OVER (PARTITION BY conversation_id ORDER BY created_at ASC, id ASC)
FROM messages;

ALTER TABLE messages RENAME TO _messages_old;
ALTER TABLE _messages_new RENAME TO messages;
DROP TABLE IF EXISTS _messages_old;

CREATE INDEX IF NOT EXISTS idx_messages_conversation_id ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created_at ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
CREATE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created ON messages(conversation_id, created_at);
CREATE INDEX IF NOT EXISTS idx_messages_conv_created_desc ON messages(conversation_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_type_created ON messages(type, created_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_conv_sequence_unique
ON messages(conversation_id, sequence);
