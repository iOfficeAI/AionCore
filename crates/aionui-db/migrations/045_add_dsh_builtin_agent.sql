-- DeepSeek Harness (dsh CLI) builtin ACP agent.
--
-- DSH natively supports the Agent Client Protocol (ACP) via `dsh --profile acp`.
-- Spoken over stdio using standard JSON-RPC 2.0 frames.
--
-- Binary name: dsh (installed globally via `npm install -g @deepseek-ai/dsh` or local build)
-- Spawns direct CLI entrypoint `dsh --profile acp`.
-- yolo_id stays NULL: ACP session permissions are handled dynamically via standard
-- session/request_permission without unverified custom yolo mode flags.
--
INSERT INTO agent_metadata
    (id, agent_id, icon, name, description, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     sort_order, created_at, updated_at)
VALUES
    ('d5e89a01', 'd5e89a01', '/api/assets/logos/ai-major/deepseek.svg', 'DeepSeek Harness',
     'DeepSeek Harness via the dsh CLI (ACP)',
     'dsh', 'acp', 'builtin', '{"binary_name":"dsh"}',
     1, 'dsh', '["--profile","acp"]', '[]',
     '[".agents/skills"]',
     '{"supports_side_question":false}',
     NULL,
     3135,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
    agent_id = excluded.agent_id,
    icon = excluded.icon,
    name = excluded.name,
    description = excluded.description,
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
