-- M9 E15: local-kb ContextProvider storage (CSBU WorkMate).
-- Default OFF. Parallel to KnowHub. NOT the skill experience library
-- (experience_articles / skill evolution). Never auto-inject into daily chat.
-- No AGPL.

CREATE TABLE IF NOT EXISTS local_kb_settings (
    user_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS local_kb_sources (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('file', 'directory', 'text')),
    label TEXT NOT NULL,
    origin_path TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_local_kb_sources_user
    ON local_kb_sources (user_id);

CREATE TABLE IF NOT EXISTS local_kb_documents (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT NOT NULL,
    body_text TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    relative_path TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (source_id) REFERENCES local_kb_sources(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_local_kb_documents_user
    ON local_kb_documents (user_id);
CREATE INDEX IF NOT EXISTS idx_local_kb_documents_source
    ON local_kb_documents (source_id);
CREATE INDEX IF NOT EXISTS idx_local_kb_documents_user_title
    ON local_kb_documents (user_id, title);
