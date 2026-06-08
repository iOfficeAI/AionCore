-- Migration 009: Builtin Codex / Claude ACP rows now resolve through managed ACP
-- artifacts instead of runtime `npx -y`.
--
-- Keep `backend` as the durable source-of-truth. Runtime resolution now derives
-- the actual command plan from managed Node + managed ACP tool artifacts, so
-- these builtin rows no longer need a persisted bridge command.

-- Claude Code (2d23ff1c)
UPDATE agent_metadata
SET command           = NULL,
    args              = '[]',
    agent_source_info = json_remove(COALESCE(agent_source_info, '{}'), '$.bridge_binary'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '2d23ff1c'
  AND agent_source = 'builtin'
  AND backend = 'claude';

-- Codex CLI (8e1acf31)
UPDATE agent_metadata
SET command           = NULL,
    args              = '[]',
    agent_source_info = json_remove(COALESCE(agent_source_info, '{}'), '$.bridge_binary'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '8e1acf31'
  AND agent_source = 'builtin'
  AND backend = 'codex';
