-- Migration 034: deleted Memory rows retain only their opaque fingerprint and lifecycle metadata.
DELETE FROM memory_sources
WHERE memory_entry_id IN (
    SELECT id FROM memory_entries WHERE state = 'deleted'
);

UPDATE memory_entries
SET stable_key = '',
    pinned = 0,
    user_edited = 0,
    supersedes_id = NULL,
    conflict_group_id = NULL
WHERE state = 'deleted'
  AND (
    stable_key <> ''
    OR pinned <> 0
    OR user_edited <> 0
    OR supersedes_id IS NOT NULL
    OR conflict_group_id IS NOT NULL
  );

CREATE TRIGGER IF NOT EXISTS memory_entries_deleted_invariant_insert
BEFORE INSERT ON memory_entries
WHEN NEW.state = 'deleted'
 AND (
    NEW.stable_key <> ''
    OR NEW.content IS NOT NULL
    OR NEW.pinned <> 0
    OR NEW.user_edited <> 0
    OR NEW.supersedes_id IS NOT NULL
    OR NEW.conflict_group_id IS NOT NULL
    OR NEW.deleted_at IS NULL
 )
BEGIN
    SELECT RAISE(ABORT, 'deleted Memory entry violates tombstone invariant');
END;

CREATE TRIGGER IF NOT EXISTS memory_entries_deleted_invariant_update
BEFORE UPDATE ON memory_entries
WHEN NEW.state = 'deleted'
 AND (
    NEW.stable_key <> ''
    OR NEW.content IS NOT NULL
    OR NEW.pinned <> 0
    OR NEW.user_edited <> 0
    OR NEW.supersedes_id IS NOT NULL
    OR NEW.conflict_group_id IS NOT NULL
    OR NEW.deleted_at IS NULL
    OR EXISTS (
        SELECT 1 FROM memory_sources WHERE memory_entry_id = OLD.id
    )
 )
BEGIN
    SELECT RAISE(ABORT, 'deleted Memory entry violates tombstone invariant');
END;

CREATE TRIGGER IF NOT EXISTS memory_sources_reject_deleted_entry_insert
BEFORE INSERT ON memory_sources
WHEN EXISTS (
    SELECT 1 FROM memory_entries
    WHERE id = NEW.memory_entry_id AND state = 'deleted'
)
BEGIN
    SELECT RAISE(ABORT, 'deleted Memory entry cannot have sources');
END;

CREATE TRIGGER IF NOT EXISTS memory_sources_reject_deleted_entry_update
BEFORE UPDATE OF memory_entry_id ON memory_sources
WHEN EXISTS (
    SELECT 1 FROM memory_entries
    WHERE id = NEW.memory_entry_id AND state = 'deleted'
)
BEGIN
    SELECT RAISE(ABORT, 'deleted Memory entry cannot have sources');
END;
