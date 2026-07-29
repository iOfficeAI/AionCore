-- Migration 031: durable normalized Memory storage and canonical turn linkage.

ALTER TABLE messages ADD COLUMN turn_id TEXT;
CREATE INDEX IF NOT EXISTS idx_messages_conversation_turn_created
    ON messages(conversation_id, turn_id, created_at);

CREATE TABLE IF NOT EXISTS memory_settings (
    user_id          TEXT PRIMARY KEY NOT NULL,
    enabled          INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN (0, 1)),
    default_capture  INTEGER NOT NULL DEFAULT 1 CHECK(default_capture IN (0, 1)),
    default_recall   INTEGER NOT NULL DEFAULT 1 CHECK(default_recall IN (0, 1)),
    consent_version  INTEGER,
    consented_at     INTEGER,
    reset_at         INTEGER,
    lifecycle_epoch  INTEGER NOT NULL DEFAULT 0 CHECK(lifecycle_epoch >= 0),
    updated_at       INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS conversation_memory_policies (
    user_id          TEXT NOT NULL,
    conversation_id  TEXT NOT NULL,
    capture_enabled  INTEGER CHECK(capture_enabled IN (0, 1)),
    recall_enabled   INTEGER CHECK(recall_enabled IN (0, 1)),
    reset_at         INTEGER,
    lifecycle_epoch  INTEGER NOT NULL DEFAULT 0 CHECK(lifecycle_epoch >= 0),
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (user_id, conversation_id),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS conversation_memories (
    user_id            TEXT NOT NULL,
    conversation_id    TEXT NOT NULL,
    project_id         TEXT,
    workspace_key      TEXT,
    summary_json       TEXT NOT NULL CHECK(json_valid(summary_json)),
    through_turn_id    TEXT NOT NULL,
    revision           INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    source             TEXT NOT NULL CHECK(source IN ('memory_update', 'legacy_context_snapshot')),
    schema_version     INTEGER NOT NULL CHECK(schema_version > 0),
    prompt_version     TEXT,
    writer_provider_id TEXT,
    writer_model_id    TEXT,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (user_id, conversation_id),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_entries (
    id                TEXT PRIMARY KEY NOT NULL,
    user_id           TEXT NOT NULL,
    project_id        TEXT,
    workspace_key     TEXT,
    kind              TEXT NOT NULL CHECK(kind IN ('decision', 'outcome', 'artifact', 'issue', 'next_step', 'work_constraint')),
    stable_key        TEXT NOT NULL,
    fingerprint       TEXT NOT NULL,
    content           TEXT,
    state             TEXT NOT NULL CHECK(state IN ('active', 'superseded', 'conflict', 'deleted')),
    pinned            INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0, 1)),
    user_edited       INTEGER NOT NULL DEFAULT 0 CHECK(user_edited IN (0, 1)),
    revision          INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    supersedes_id     TEXT,
    conflict_group_id TEXT,
    schema_version    INTEGER NOT NULL CHECK(schema_version > 0),
    deleted_at        INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    CHECK((state = 'deleted' AND content IS NULL AND deleted_at IS NOT NULL)
       OR (state <> 'deleted' AND content IS NOT NULL AND deleted_at IS NULL)),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (supersedes_id) REFERENCES memory_entries(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS memory_sources (
    memory_entry_id   TEXT NOT NULL,
    conversation_id  TEXT NOT NULL,
    turn_id           TEXT NOT NULL,
    message_ids_json  TEXT NOT NULL CHECK(json_valid(message_ids_json) AND json_type(message_ids_json) = 'array'),
    first_observed_at INTEGER NOT NULL,
    last_observed_at  INTEGER NOT NULL,
    PRIMARY KEY (memory_entry_id, conversation_id, turn_id),
    FOREIGN KEY (memory_entry_id) REFERENCES memory_entries(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_change_sets (
    id                  TEXT PRIMARY KEY NOT NULL,
    user_id             TEXT NOT NULL,
    conversation_id     TEXT NOT NULL,
    through_turn_id     TEXT NOT NULL,
    job_id              TEXT NOT NULL,
    added_ids_json      TEXT NOT NULL CHECK(json_valid(added_ids_json) AND json_type(added_ids_json) = 'array'),
    refined_ids_json    TEXT NOT NULL CHECK(json_valid(refined_ids_json) AND json_type(refined_ids_json) = 'array'),
    superseded_ids_json TEXT NOT NULL CHECK(json_valid(superseded_ids_json) AND json_type(superseded_ids_json) = 'array'),
    conflict_ids_json   TEXT NOT NULL CHECK(json_valid(conflict_ids_json) AND json_type(conflict_ids_json) = 'array'),
    created_at          INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_jobs (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_id            TEXT NOT NULL,
    conversation_id    TEXT NOT NULL,
    from_turn_id       TEXT,
    through_turn_id    TEXT NOT NULL,
    operation_version  TEXT NOT NULL,
    global_epoch       INTEGER NOT NULL DEFAULT 0 CHECK(global_epoch >= 0),
    conversation_epoch INTEGER NOT NULL DEFAULT 0 CHECK(conversation_epoch >= 0),
    turn_count         INTEGER NOT NULL DEFAULT 0 CHECK(turn_count >= 0),
    queue_digest       TEXT NOT NULL,
    input_hash         TEXT NOT NULL,
    expected_revision  INTEGER NOT NULL CHECK(expected_revision >= 0),
    state              TEXT NOT NULL CHECK(state IN ('pending', 'running', 'retry_wait', 'blocked', 'succeeded', 'failed', 'canceled')),
    attempt_count      INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    next_attempt_at    INTEGER,
    lease_owner        TEXT,
    lease_token        TEXT,
    lease_expires_at   INTEGER,
    invalid_output_count INTEGER NOT NULL DEFAULT 0 CHECK(invalid_output_count >= 0),
    reconciliation_snapshot_json TEXT CHECK(
        reconciliation_snapshot_json IS NULL
        OR (json_valid(reconciliation_snapshot_json) AND json_type(reconciliation_snapshot_json) = 'array')
    ),
    last_error_code    TEXT,
    created_at         INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    UNIQUE (user_id, conversation_id, through_turn_id, operation_version),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_job_turns (
    job_id             TEXT NOT NULL,
    user_id            TEXT NOT NULL,
    conversation_id    TEXT NOT NULL,
    operation_version  TEXT NOT NULL,
    position           INTEGER NOT NULL CHECK(position >= 0),
    turn_id            TEXT NOT NULL,
    turn_hash          TEXT NOT NULL,
    PRIMARY KEY (job_id, position),
    UNIQUE (user_id, conversation_id, operation_version, turn_id),
    FOREIGN KEY (job_id) REFERENCES memory_jobs(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_retrievals (
    id                  TEXT PRIMARY KEY NOT NULL,
    user_id             TEXT NOT NULL,
    conversation_id     TEXT NOT NULL,
    prompt_hash         TEXT NOT NULL,
    selected_ids_json   TEXT NOT NULL CHECK(json_valid(selected_ids_json) AND json_type(selected_ids_json) = 'array'),
    estimated_tokens    INTEGER NOT NULL CHECK(estimated_tokens >= 0),
    budget_tokens       INTEGER NOT NULL CHECK(budget_tokens >= 0),
    retrieval_version   TEXT NOT NULL,
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS memory_import_state (
    user_id        TEXT PRIMARY KEY NOT NULL,
    cursor         TEXT,
    completed      INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0, 1)),
    started_at     INTEGER,
    completed_at   INTEGER,
    updated_at     INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_settings_reset ON memory_settings(reset_at);
CREATE INDEX IF NOT EXISTS idx_memory_policies_conversation ON conversation_memory_policies(conversation_id);
CREATE INDEX IF NOT EXISTS idx_memory_conversations_scope_updated
    ON conversation_memories(user_id, project_id, workspace_key, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_entries_user_state_scope_updated
    ON memory_entries(user_id, state, project_id, workspace_key, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_entries_kind_created
    ON memory_entries(user_id, kind, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_entries_fingerprint
    ON memory_entries(user_id, fingerprint);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_entries_one_active_fingerprint
    ON memory_entries(user_id, fingerprint) WHERE state = 'active';
CREATE INDEX IF NOT EXISTS idx_memory_sources_conversation
    ON memory_sources(conversation_id, turn_id, memory_entry_id);
CREATE INDEX IF NOT EXISTS idx_memory_change_sets_user_created
    ON memory_change_sets(user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_jobs_claim
    ON memory_jobs(state, next_attempt_at, created_at, id);
CREATE INDEX IF NOT EXISTS idx_memory_jobs_user_state
    ON memory_jobs(user_id, state, updated_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_jobs_one_running
    ON memory_jobs(user_id, conversation_id) WHERE state = 'running';
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_jobs_one_next
    ON memory_jobs(user_id, conversation_id) WHERE state IN ('pending', 'retry_wait', 'blocked');
CREATE UNIQUE INDEX IF NOT EXISTS idx_memory_jobs_lease_token
    ON memory_jobs(lease_token) WHERE lease_token IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memory_job_turns_job_position
    ON memory_job_turns(job_id, position);
CREATE INDEX IF NOT EXISTS idx_memory_retrievals_expiry
    ON memory_retrievals(expires_at);
CREATE INDEX IF NOT EXISTS idx_memory_import_pending
    ON memory_import_state(completed, updated_at);
