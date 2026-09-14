-- Add MiniMax Code as a builtin ACP agent (Registry-listed, npx distribution).
--
-- Identity: the public Registry card is "MiniMax Code" (https://agent.minimax.io);
-- the npm package is @minimax-ai/code (bin `mcode`); a live ACP `initialize`
-- reports agentInfo {name:"minimax-code", title:"MiniMax Code"}. `backend` is
-- taken from the product name plus the agent's own reported name, following the
-- `mimo-code` precedent (029) for "<vendor> Code" products. It is not a copy of
-- the CDN JSON lookup id, which merely coincides.
--
-- `binary_name` is `mcode`: the official README installs the product with
-- `npm install -g @minimax-ai/code`, which places `mcode` on PATH, and
-- `mcode --version` is documented, so the default PATH probe applies.
-- `mcode acp` is the vendor's documented ACP server (README "ACP clients");
-- the Registry declares the same `acp` argument. AionCore launches the
-- Registry npx distribution; the exact version is pinned in
-- crates/aionui-runtime/resources/acp-registry-npx-lock.json, never here.
--
-- Probe evidence (2026-09-14, @minimax-ai/code@0.2.7, clean temporary HOME):
--   initialize ok, protocolVersion 1; session/new -> -32000
--   "Authentication required: Run `mcode login` and try again."
-- agent_capabilities is the snake_case form of what initialize returned
-- (contract: migration 003 header). auth_methods stays NULL: initialize
-- advertised none, and a blob must not be synthesized.
--
-- yolo_id stays NULL. The 0.2.7 ACP server exposes exactly two session modes,
-- `default` and `plan` (source: run-acp-command chunk, availableModes). The
-- Ask / Auto / Full access permission levels are a `_permission`-category
-- CONFIG OPTION (`permissionMode` = default | auto | bypassPermissions), not a
-- session mode, and AionCore resolves yolo_id through `session/set_mode`, so
-- storing `bypassPermissions` here would send an unsupported mode id.
--
-- native_skills_dirs stays NULL. The 0.2.7 source scans only
-- `$MINIMAX_DATA_DIR/skills` (a user data directory) and the bundled
-- `assets/skills`; there is no project-relative skills directory to declare.
--
-- behavior_policy omits `supports_team`: migration 033 retired the negative
-- form, and team capability is derived from backend + probed capabilities.
-- Post-030 seed shape: builtin rows use agent_id = id and user_id NULL.
INSERT INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, sort_order, created_at, updated_at)
VALUES
    ('ec619063', 'ec619063', '/api/assets/logos/acp-registry/minimax-code.svg', 'MiniMax Code',
     'minimax-code', 'acp', 'builtin', '{"binary_name":"mcode","bridge_binary":"npx"}',
     1, 'npx', '["-y","@minimax-ai/code","acp"]', '[]',
     NULL,
     '{"supports_side_question":false}',
     NULL,
     '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":false,"audio":false,"embedded_context":false},"session_capabilities":{"list":{},"fork":{},"resume":{},"close":{}}}',
     3340,
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
