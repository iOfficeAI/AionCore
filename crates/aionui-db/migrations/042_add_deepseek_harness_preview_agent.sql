-- DeepSeek Harness Preview is installed into AionCore's versioned managed
-- runtime, not discovered on PATH. The verified ACP contract was captured from
-- @deepseek-ai/dsh-acp-demo@0.0.1-rc.5 using initialize + session/new.
INSERT OR IGNORE INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, auth_methods, sort_order, created_at, updated_at)
VALUES
    ('d54a7e91', 'd54a7e91', '/api/assets/logos/ai-major/deepseek.svg',
     'DeepSeek Harness', 'deepseek-harness', 'acp', 'builtin',
     '{"managed_runtime":{"runtime_id":"deepseek-harness","release":"2026.08.14-1"}}',
     1, 'node', '[]', '[]', '[".agents/skills",".claude/skills"]',
     '{"supports_side_question":false,"session_lifetime":"connection_scoped"}',
     NULL,
     '{"prompt_capabilities":{"image":false,"audio":false,"embedded_context":false}}',
     '[]', 3340,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);
