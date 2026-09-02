-- Add DeepSeek Harness (dsh-catl-plugins) as a builtin ACP agent.
--
-- Local CLI entry: node + absolute path to dsh-catl-plugins/scripts/run.mjs
-- Out-of-tree ACP bridge (streaming + usage_update + egress guard + Team MCP).
-- Requires DEEPSEEK_API_KEY in the process env (or agent_metadata.env) — never
-- commit secrets.
--
-- command/args use a portable placeholder; operators MUST set args to the
-- absolute path of scripts/run.mjs on this host (AionUi Agent settings), and
-- set DSH_ROOT / DEEPSEEK_API_KEY in agent_metadata.env.
--
-- Post-030 seed shape: builtin rows use agent_id = id and user_id NULL.
INSERT INTO agent_metadata
    (id, agent_id, icon, name, description, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, agent_capabilities,
     yolo_id, sort_order, created_at, updated_at)
VALUES
    ('d5e0a101', 'd5e0a101', '/api/assets/logos/ai-major/deepseek.svg', 'DeepSeek Harness',
     'DeepSeek Harness via dsh-catl-plugins ACP CLI (streaming + usage + egress + Team MCP). Set args to absolute run.mjs; set DSH_ROOT + DEEPSEEK_API_KEY in env.',
     'deepseek', 'acp', 'builtin', '{"binary_name":"node","bridge_binary":"node"}',
     1, 'node',
     '["/path/to/dsh-catl-plugins/scripts/run.mjs"]',
     '[]',
     '[".agents/skills"]',
     '{"supports_side_question":false,"supports_team":true}',
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"audio":false,"embedded_context":false}}',
     NULL, 3200,
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
    agent_capabilities = excluded.agent_capabilities,
    yolo_id = excluded.yolo_id,
    sort_order = excluded.sort_order,
    updated_at = unixepoch('now','subsec')*1000;
