-- Add Pi through the pi-acp adapter listed in the official ACP Registry.
--
-- Registry distribution:
-- https://github.com/agentclientprotocol/registry/blob/main/pi-acp/agent.json
-- Verified initialization against pi-acp 0.0.31 and Pi 0.80.7:
-- ~/aion/protocols/samples/pi-acp/0.0.31/initialize-list.ndjson
-- Pi's lack of mcpServers support:
-- https://github.com/svkozak/pi-acp/blob/v0.0.31/src/acp/agent.ts
INSERT INTO agent_metadata
    (id, icon, name, description, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, auth_methods, sort_order, created_at, updated_at)
VALUES
    ('484e4bf2', '/api/assets/logos/tools/pi.svg', 'Pi',
     'Pi coding agent through the pi-acp adapter',
     'pi', 'acp', 'builtin', '{"binary_name":"pi","bridge_binary":"npx","version":"0.0.31"}',
     1, 'npx', '["-y","pi-acp@0.0.31"]', '[]',
     '[".pi/skills"]',
     '{"supports_side_question":false,"supports_team":false,"team_capable_override":false}',
     NULL,
     '{"load_session":true,"mcp_capabilities":{"http":false,"sse":false},"prompt_capabilities":{"image":true,"audio":false,"embedded_context":false},"session_capabilities":{"list":{}}}',
     '[{"id":"pi_terminal_login","name":"Launch pi in the terminal","description":"Start pi in an interactive terminal to configure API keys or login","type":"terminal","args":["--terminal-login"],"env":{}}]',
     3130,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO UPDATE SET
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
    agent_capabilities = excluded.agent_capabilities,
    auth_methods = excluded.auth_methods,
    sort_order = excluded.sort_order,
    updated_at = unixepoch('now','subsec')*1000;
