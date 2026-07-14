-- Migration 021: Canonicalize builtin Codex ACP full-access mode metadata.
UPDATE agent_metadata
SET yolo_id = 'agent-full-access',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE agent_source = 'builtin'
  AND agent_type = 'acp'
  AND backend = 'codex'
  AND yolo_id = 'full-access';
