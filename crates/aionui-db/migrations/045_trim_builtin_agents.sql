-- Keep only Aion CLI, OpenCode, Pi, and DeepSeek Harness as builtin/internal agents.
-- Custom agents (agent_source = 'custom') are left untouched.
-- Generated assistants whose source_ref no longer points at a live agent are soft-deleted.

DELETE FROM agent_metadata
WHERE agent_source IN ('builtin', 'internal')
  AND agent_id NOT IN ('632f31d2', '53861a53', '484e4bf2', 'd5e0a101');

UPDATE assistant_definitions
SET deleted_at = unixepoch('now', 'subsec') * 1000,
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE source = 'generated'
  AND deleted_at IS NULL
  AND source_ref IS NOT NULL
  AND source_ref NOT IN (SELECT agent_id FROM agent_metadata);
