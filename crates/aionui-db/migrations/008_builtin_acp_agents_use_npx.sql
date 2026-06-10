-- Migration 008: Move builtin bridged ACP agents from `bun x --bun` to `npx`.
--
-- Managed Node runtime is now the supported bridge path for npm-based ACP
-- launchers. Keep exact package versions pinned so one AionUi release remains
-- reproducible across installs and restarts.

-- Claude Code (2d23ff1c)
UPDATE agent_metadata
SET command           = 'npx',
    args              = '["-y","@agentclientprotocol/claude-agent-acp@0.39.0"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npx'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '2d23ff1c'
  AND agent_source = 'builtin'
  AND backend = 'claude'
  AND command = 'bun';

-- Codex CLI (8e1acf31)
UPDATE agent_metadata
SET command           = 'npx',
    args              = '["-y","@zed-industries/codex-acp@0.14.0"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npx'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '8e1acf31'
  AND agent_source = 'builtin'
  AND backend = 'codex'
  AND command = 'bun';

-- CodeBuddy (8b20fd41)
UPDATE agent_metadata
SET command           = 'npx',
    args              = '["-y","--package","@tencent-ai/codebuddy-code@2.97.0","codebuddy","--acp"]',
    agent_source_info = json_set(COALESCE(agent_source_info, '{}'), '$.bridge_binary', 'npx'),
    updated_at        = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '8b20fd41'
  AND agent_source = 'builtin'
  AND backend = 'codebuddy'
  AND command = 'bun';
