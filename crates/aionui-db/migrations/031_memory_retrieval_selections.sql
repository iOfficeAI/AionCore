CREATE TABLE IF NOT EXISTS memory_retrieval_selections (
    retrieval_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    selection_id TEXT NOT NULL,
    selection_kind TEXT NOT NULL CHECK (selection_kind IN ('entry', 'conversation_summary')),
    snapshot_hash TEXT NOT NULL CHECK (length(snapshot_hash) = 64),
    PRIMARY KEY (retrieval_id, position),
    UNIQUE (retrieval_id, selection_id),
    FOREIGN KEY (retrieval_id) REFERENCES memory_retrievals (id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_retrieval_selections_selection
    ON memory_retrieval_selections (selection_id, retrieval_id);
