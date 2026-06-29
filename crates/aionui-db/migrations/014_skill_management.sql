-- Migration 014: skill management metadata and import history
--
-- `skills` is the source of truth for skill listing and user-managed skill state.
-- Skill files still live on disk under the data directory; the database owns
-- listing, soft deletion, and path lookup semantics.

CREATE TABLE IF NOT EXISTS skills (
    id          TEXT    PRIMARY KEY NOT NULL,
    name        TEXT    NOT NULL UNIQUE,
    description TEXT,
    path        TEXT    NOT NULL,
    source      TEXT    NOT NULL DEFAULT 'user'
                            CHECK (source IN ('user', 'builtin', 'extension', 'cron')),
    enabled     INTEGER NOT NULL DEFAULT 1,
    deleted_at  INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_skills_deleted_at ON skills(deleted_at);
CREATE INDEX IF NOT EXISTS idx_skills_source ON skills(source);
CREATE INDEX IF NOT EXISTS idx_skills_updated_at ON skills(updated_at DESC);

CREATE TABLE IF NOT EXISTS skill_import_records (
    id           TEXT    PRIMARY KEY NOT NULL,
    operation_id TEXT    NOT NULL,

    source_label TEXT    NOT NULL,
    source_path  TEXT,
    source_name  TEXT    NOT NULL,

    skill_id     TEXT,
    skill_name   TEXT,

    status       TEXT    NOT NULL
                          CHECK (status IN ('imported', 'failed', 'overwritten')),
    error_code   TEXT,

    error_path   TEXT,
    actual_bytes INTEGER,
    limit_bytes  INTEGER,
    line         INTEGER,
    column       INTEGER,

    created_at   INTEGER NOT NULL,

    FOREIGN KEY (skill_id) REFERENCES skills(id)
);

CREATE INDEX IF NOT EXISTS idx_skill_import_records_operation_id ON skill_import_records(operation_id);
CREATE INDEX IF NOT EXISTS idx_skill_import_records_created_at ON skill_import_records(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_skill_import_records_status ON skill_import_records(status);
CREATE INDEX IF NOT EXISTS idx_skill_import_records_skill_id ON skill_import_records(skill_id);
