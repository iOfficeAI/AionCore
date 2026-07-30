-- Add omp (Oh My Pi) as a builtin ACP agent.
--
-- omp is a terminal coding agent (pi-mono fork) with a native ACP
-- entrypoint (`omp acp`, launched here via
-- `npx -y @oh-my-pi/pi-coding-agent acp`): https://github.com/can1357/oh-my-pi
--   packages/coding-agent/src/commands/acp.ts  (ACP server over stdio)
--   packages/coding-agent/src/modes/acp/       (agent, event mapper, client bridge)
-- Not listed on the public ACP Registry as of 2026-07-29; integrated from
-- product/source evidence plus a local ACP probe of
-- @oh-my-pi/pi-coding-agent@17.1.8: initialize ok (protocolVersion 1,
-- agentInfo oh-my-pi "Oh My Pi" 17.1.8), session/new ok unauthenticated
-- with the default/plan mode catalog. `omp --version` prints omp/17.1.8,
-- so the default PATH probe applies.
-- Skills: discovery/builtin.ts scans project `<root>/.omp/skills`
-- (CONFIG_DIR_NAME ".omp", packages/utils/src/dirs.ts) and user
-- `~/.omp/agent/skills`; discovery/claude.ts scans `.claude/skills`.
-- Persist the omp-native project dir first with the Claude-compat dir as
-- fallback.
-- yolo_id stays NULL: ACP session modes are default/plan only, and the ACP
-- permission gate (src/session/acp-permission-gate.ts) always gates
-- bash/edit/delete/move; `--yolo`/`--auto-approve` are CLI launch flags,
-- which must not be recorded as an ACP mode id.
-- Display name is the product brand "omp" (omp.sh, CLI `omp`) rather than
-- the ACP agentInfo.title "Oh My Pi", so users searching the CLI name find it.
-- Post-030 seed shape: builtin rows use agent_id = id and user_id NULL
-- (agents are machine-level; see 030_user_scope.sql backfill).
INSERT INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('c9e8a2f4', 'c9e8a2f4', '/api/assets/logos/acp-registry/omp.svg', 'omp',
     'omp', 'acp', 'builtin', '{"binary_name":"omp","bridge_binary":"npx"}',
     1, 'npx', '["-y","@oh-my-pi/pi-coding-agent","acp"]', '[]',
     '[".omp/skills",".claude/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',
     NULL, 3330,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
    agent_id = excluded.agent_id,
    icon = excluded.icon,
    name = excluded.name,
    description = NULL,
    backend = excluded.backend,
    agent_type = excluded.agent_type,
    agent_source = excluded.agent_source,
    agent_source_info = excluded.agent_source_info,
    enabled = excluded.enabled,
    command = excluded.command,
    args = excluded.args,
    env = excluded.env,
    native_skills_dirs = excluded.native_skills_dirs,
    behavior_policy = excluded.behavior_policy,
    yolo_id = excluded.yolo_id,
    sort_order = excluded.sort_order,
    updated_at = unixepoch('now','subsec')*1000;
