-- Migration 011: Add OpenClaw as an ACP builtin backend.
--
-- Keep the legacy openclaw-gateway row for historical read-only conversations.
-- New OpenClaw conversations must use agent_type='acp' with backend='openclaw'.

INSERT INTO agent_metadata
    (id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('b7e8a9c4', '/api/assets/logos/tools/openclaw.svg', 'OpenClaw',
     'openclaw', 'acp', 'builtin', '{"binary_name":"openclaw"}',
     1, 'openclaw', '["acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     NULL, 3140,
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
