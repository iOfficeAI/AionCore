-- Let the runtime infer team capability for all builtin ACP agents
-- instead of blocking them with a blanket team_capable_override:false.
--
-- Migrations 023 (Pi), 025 (ACP Registry npx + binary agents), and 029
-- (Mimo Code) all seed behavior_policy with team_capable_override:false
-- as a conservative default. This blocks every external ACP agent from
-- joining teams, even when supports_team_cli_fallback() would naturally
-- return true for agents that don't declare shell:false or cli:false in
-- their handshake.
--
-- The correct gate is the runtime inference in is_team_capable():
--   ─ supports_team_mcp() for agents with MCP transport (stdio/http)
--   ─ supports_team_cli_fallback() for agents that can execute shell
--     commands (true unless caps explicitly disable shell/cli)
--
-- Removing the override lets each agent's actual capabilities decide.
-- Agents that later gain MCP support will automatically use the richer
-- Mcp transport via team_tool_transport().
UPDATE agent_metadata
SET behavior_policy = json_remove(behavior_policy, '$.team_capable_override'),
    updated_at = unixepoch('now','subsec') * 1000
WHERE agent_source = 'builtin'
  AND agent_type = 'acp';
