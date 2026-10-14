-- Add IBM Bob as a builtin ACP agent (Registry-listed, npx distribution).
--
-- Identity: the public Registry card is "Bob" (https://bob.ibm.com, "IBM Bob is
-- your AI-Powered Development Partner"); the npm package is @ibm/bob (homepage
-- https://bob.ibm.com/, bin `bob`); a live ACP `initialize` reports agentInfo
-- {name:"bob-shell", title:"Bob"}. `backend` is the product name, which is also
-- the canonical CLI the vendor documents. It is not a copy of the CDN JSON
-- lookup id, which merely coincides.
--
-- `binary_name` is `bob`: the package README installs the product with
-- `npm install -g @ibm/bob`, which places `bob` on PATH, and documents
-- `-v, --version`. `bob --version` was run with a clean HOME and exited 0
-- within about a second once cached, printing "2.0.7" then a "commit:" line
-- (the availability probe keeps the first line), so the default PATH probe
-- applies and the release lock needs no `skip_version_probe`. `bob acp` is
-- the vendor's documented ACP server (package ACP.md); the Registry declares the same
-- `acp` argument. AionCore launches the Registry npx distribution; the exact
-- version is pinned in crates/aionui-runtime/resources/acp-registry-npx-lock.json,
-- never here.
--
-- Probe evidence (2026-10-15, @ibm/bob@2.0.7, clean temporary HOME):
--   initialize ok, protocolVersion 1, agentInfo bob-shell 2.0.7.
--   session/new with the Registry argv -> -32600 "Workspace ... is not
--   trusted. Pass --trust to trust each workspace opened by this ACP server,
--   or run Bob Shell interactively in this folder and choose a trust level".
--   session/new with `acp --trust` -> -32000 "Authentication required".
-- The stored argv is the Registry's, without `--trust`: auto-trusting every
-- workspace the ACP server opens is a security decision this seed does not
-- make. Until it is made, a user trusts a folder once by running Bob Shell in
-- it, as the vendor's own error message instructs.
--
-- agent_capabilities and auth_methods are the snake_case form of what
-- initialize returned (contract: migration 003 header); `_meta` inner keys
-- stay verbatim. Neither blob embeds a host path.
--
-- yolo_id stays NULL. Bob ships three session modes in source -- `agent`
-- (the default), `plan` and `ask` -- and none bypasses approvals. Its
-- ACP.md is explicit that "protected operations still require explicit
-- approval" even under `--auto-approve`, which is a launch flag, not a mode.
-- The NULL falls back to `AgentType::full_auto_mode_id`'s `"yolo"`, which
-- Bob does not advertise. No invalid mode reaches the wire: ACP reconcile
-- drops a desired mode the session does not advertise instead of sending it
-- (`AcpSession::clear_invalid_desired_mode`), and cron's full-auto path only
-- applies a mode that is in the live catalog. Two consequences remain and are
-- NOT addressed by this seed (both are shared with every agent whose yolo_id
-- is NULL and has no `full_auto_mode_id` arm, e.g. minimax-code): a
-- cron-bound conversation persists "yolo" as its runtime mode and a
-- `session/load` resume then reports it as the current mode (state only); and
-- a Bob teammate runs in `agent`, so tool calls that AionCore does not
-- auto-approve still prompt.
--
-- native_skills_dirs is a PROVISIONAL source-level mapping, not an end-to-end
-- load (that needs an authenticated session). Bob's WorkspaceSkillsManager
-- scans the workspace paths `.bob/skills`, `.bob/plugins/*/skills`,
-- `.agents/skills` and `.claude/skills`, and the bundled guidance names
-- `.bob` as the canonical location. Only the vendor's own directory is
-- declared. skill_delivery stays NULL, i.e. the safe `injected` default
-- (migration 043): the declared directory does not change how skills are
-- delivered.
--
-- behavior_policy omits `supports_team`: migration 033 retired the negative
-- form, and team capability is derived from backend + probed capabilities.
-- Post-030 seed shape: builtin rows use agent_id = id and user_id NULL.
INSERT INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     agent_capabilities, auth_methods, sort_order, created_at, updated_at)
VALUES
    ('be4da246', 'be4da246', '/api/assets/logos/acp-registry/bob.svg', 'Bob',
     'bob', 'acp', 'builtin', '{"binary_name":"bob","bridge_binary":"npx"}',
     1, 'npx', '["-y","@ibm/bob","acp"]', '[]',
     '[".bob/skills"]',
     '{"supports_side_question":false}',
     NULL,
     '{"_meta":{"bob/extensions":{"version":1}},"load_session":true,"session_capabilities":{"list":{},"delete":{},"resume":{},"close":{}},"prompt_capabilities":{"embedded_context":true,"image":true},"mcp_capabilities":{"http":true}}',
     '[{"id":"sso","name":"Log in with SSO","description":"Opens your browser to authenticate with IBM SSO"}]',
     3360,
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
