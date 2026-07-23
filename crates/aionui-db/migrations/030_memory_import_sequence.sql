-- Migration 030: non-reusable conversation membership for bounded legacy Memory import snapshots.

CREATE TABLE conversation_memory_import_sequences (
    conversation_id TEXT PRIMARY KEY NOT NULL,
    user_id          TEXT NOT NULL,
    sequence         INTEGER NOT NULL UNIQUE CHECK(sequence > 0),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX idx_conversation_memory_import_sequences_user
    ON conversation_memory_import_sequences(user_id, sequence);

CREATE TABLE memory_import_sequence_counter (
    singleton     INTEGER PRIMARY KEY NOT NULL CHECK(singleton = 1),
    next_sequence INTEGER NOT NULL CHECK(next_sequence > 0)
);

INSERT INTO memory_import_sequence_counter (singleton, next_sequence) VALUES (1, 1);

INSERT INTO conversation_memory_import_sequences (conversation_id, user_id, sequence)
SELECT id, user_id, ROW_NUMBER() OVER (ORDER BY rowid)
FROM conversations
ORDER BY rowid;

UPDATE memory_import_sequence_counter
SET next_sequence = COALESCE(
    (SELECT MAX(sequence) + 1 FROM conversation_memory_import_sequences),
    1
)
WHERE singleton = 1;

CREATE TRIGGER conversations_assign_memory_import_sequence
AFTER INSERT ON conversations
BEGIN
    INSERT INTO conversation_memory_import_sequences (conversation_id, user_id, sequence)
    SELECT NEW.id, NEW.user_id, next_sequence
    FROM memory_import_sequence_counter
    WHERE singleton = 1;

    UPDATE memory_import_sequence_counter
    SET next_sequence = next_sequence + 1
    WHERE singleton = 1;
END;
