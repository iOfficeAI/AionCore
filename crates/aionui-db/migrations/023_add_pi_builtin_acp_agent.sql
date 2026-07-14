-- Migration 023: Add Pi coding agent as a builtin ACP agent.
--
-- Pi (https://pi.dev/) is a CLI coding agent installed via npm.
-- It is bridged to ACP through pi-acp (npm: pi-acp), which runs via `npx -y pi-acp`.
-- No global install of pi-acp is required; npx fetches and runs the latest version.
--
-- The sort_order of 3130 places Pi alongside Qwen, Goose, Droid, OpenCode,
-- Copilot, and similar third-party ACP agents.

INSERT INTO agent_metadata
    (id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('484e4bf2', '/api/assets/logos/tools/pi.svg', 'Pi',
     'pi', 'acp', 'builtin', '{"binary_name":"pi","bridge_binary":"npx"}',
     1, 'npx', '["-y","pi-acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     NULL, 3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
    icon = excluded.icon,
    name = excluded.name,
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
