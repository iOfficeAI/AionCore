-- Add MiMo Code (Xiaomi) as a builtin ACP agent.
--
-- MiMo Code is an OpenCode fork that keeps the inherited native ACP
-- entrypoint (`mimo acp`, launched here via `npx -y @mimo-ai/cli acp`):
-- https://github.com/XiaomiMiMo/MiMo-Code
--   packages/opencode/src/cli/cmd/acp.ts  (AgentSideConnection over stdio)
--   packages/opencode/src/index.ts        (AcpCommand registered)
-- Not listed on the public ACP Registry as of 2026-07-28; integrated from
-- product/source evidence plus a local ACP probe of @mimo-ai/cli@0.1.9:
-- initialize ok (protocolVersion 1), session/new ok with the inherited
-- build/plan/compose mode catalog. `mimo --version` prints the version,
-- so the default PATH probe applies.
-- Skills: packages/opencode/src/skill/index.ts scans the product's own
-- project `.mimocode/{skill,skills}` config dirs (config/paths.ts) plus
-- the inherited `.opencode/.claude/.agents/.codex` skills dirs
-- (EXTERNAL_DIRS); persist the MiMo-native dir first with the
-- OpenCode-compat dir as fallback.
-- yolo_id 'build' mirrors the established OpenCode ruling; the probe
-- confirmed the identical inherited mode catalog.
INSERT INTO agent_metadata
    (id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('8f21c6d3', '/api/assets/logos/acp-registry/mimo-code.svg', 'MiMo Code',
     'mimo-code', 'acp', 'builtin', '{"binary_name":"mimo","bridge_binary":"npx"}',
     1, 'npx', '["-y","@mimo-ai/cli","acp"]', '[]',
     '[".mimocode/skills",".opencode/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',
     'build', 3320,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
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
