-- Migration 020: Move any remaining builtin Codex ACP rows off the deprecated
-- @zed-industries bridge package. Runtime resolution derives builtin Codex from
-- managed ACP artifacts, where Codex ACP is pinned to
-- @agentclientprotocol/codex-acp@1.1.2, so no npm bridge command should remain
-- persisted in agent_metadata.

UPDATE agent_metadata
SET command           = NULL,
    args              = '[]',
    agent_source_info = json_remove(COALESCE(agent_source_info, '{}'), '$.bridge_binary'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE agent_source = 'builtin'
  AND backend = 'codex'
  AND args LIKE '%@zed-industries/codex-acp%';
