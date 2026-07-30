-- Migration 033: retire the team-capability veto flag and seed the handshake
-- capabilities the ACP Registry sync captured but never persisted.
--
-- Two defects, one root: since 023/025 every Registry-synced builtin shipped
-- `behavior_policy.team_capable_override = false`, a HARD veto that beat every
-- inference, while `agent_capabilities` was left NULL even though the
-- integration probe had already read it from `initialize`. The result was a
-- permanently team-blocked agent whose block could not be lifted (builtin rows
-- reject metadata edits) and whose real MCP capability was unknown to the
-- backend until the agent first connected.
--
-- 1) Drop `team_capable_override` from every behavior_policy. The veto branch is
--    gone from the hydrate path, so a leftover key would be silently ignored;
--    removing it keeps the stored policy honest about what the code reads.
--
-- 2) Drop only the `supports_team: false` entries. The flag is an OR-term over
--    the handshake inference, so `false` never denied anything — it just read
--    like a decision. `true` entries STAY: they are the known-good whitelist
--    (migration 014) and are load-bearing whenever `agent_capabilities` is still
--    NULL, which on a fresh install is the case for claude/codex/gemini and for
--    aionrs (whose backend is NULL, so the inference cannot judge it at all).
--
-- 3) Seed `agent_capabilities` for every agent the Registry-sync workflow added
--    since 023 and left NULL: the 9 npx rows of 025, plus 029 (mimo-code) and 031
--    (omp). 023 (pi) already seeded its own and needs nothing here.
--
--    All values come from a LIVE probe (2026-07-30): ACP `initialize` only — no
--    session/new, so no login was required — run against the npx versions pinned
--    in acp-registry-npx-lock.json and, for binary distributions, against the
--    product CLIs the workflow requires installing at integration time. Only rows
--    still NULL are written, so a value already learned from a real handshake is
--    never clobbered; the runtime overwrites the seed on the next connect.
--
-- Rows are addressed by `agent_id`, the stable identity every install shares
-- (builtin seeds set `agent_id = id`); `backend` is a display-adjacent field that
-- earlier migrations have already renamed and normalized, so it is not a safe key.

UPDATE agent_metadata
SET behavior_policy = json_remove(behavior_policy, '$.team_capable_override'),
    updated_at = unixepoch('now','subsec')*1000
WHERE behavior_policy IS NOT NULL
  AND json_valid(behavior_policy)
  AND json_extract(behavior_policy, '$.team_capable_override') IS NOT NULL;

UPDATE agent_metadata
SET behavior_policy = json_remove(behavior_policy, '$.supports_team'),
    updated_at = unixepoch('now','subsec')*1000
WHERE behavior_policy IS NOT NULL
  AND json_valid(behavior_policy)
  AND json_extract(behavior_policy, '$.supports_team') = 0;

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":false,"sse":false},"session_capabilities":{"modes":true,"commands":true}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'a11632d4' AND agent_capabilities IS NULL;  -- deepagents

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{"list":{},"resume":{},"close":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'd14634b4' AND agent_capabilities IS NULL;  -- dimcode

-- dirac advertises no `mcp_capabilities` object at all (its extensions ride in
-- `_meta`), so Team keeps it on the CLI transport until a probe proves MCP. Its
-- vendor `_meta` block is omitted here: nothing in the backend reads it, and the
-- next real handshake restores it verbatim.
UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"providers":{},"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"session_capabilities":{"resume":{},"close":{},"delete":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '6a95eb4f' AND agent_capabilities IS NULL;  -- dirac

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"resume":{},"close":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '5431c523' AND agent_capabilities IS NULL;  -- glm-acp-agent

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"resume":{},"close":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '54c5ccf0' AND agent_capabilities IS NULL;  -- kilo

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"search":{},"resume":{},"close":{},"delete":{},"tools":{"list":{}}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '19e05df6' AND agent_capabilities IS NULL;  -- nova

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":false,"audio":false,"embedded_context":false},"mcp_capabilities":{"http":false,"sse":false},"session_capabilities":{"fork":{}},"auth":{}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '79767ac2' AND agent_capabilities IS NULL;  -- sigit

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"resume":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'b3252207' AND agent_capabilities IS NULL;  -- autohand

-- grok's `_meta` carries x.ai hook/tool-override extensions; omitted for the same
-- reason as dirac's (unread by the backend, restored by the next handshake).
UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":false,"audio":false,"embedded_context":true},"session_capabilities":{},"auth":{}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '145a4e5c' AND agent_capabilities IS NULL;  -- grok

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"resume":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '8f21c6d3' AND agent_capabilities IS NULL;  -- mimo-code

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"image":true,"embedded_context":true},"session_capabilities":{"list":{},"fork":{},"resume":{},"close":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'c9e8a2f4' AND agent_capabilities IS NULL;  -- omp

-- Binary-distributed agents from 025, probed against the product CLIs this
-- workspace has installed (the skill requires installing them at integration
-- time). Vendor `_meta` extension blocks are omitted as above.

UPDATE agent_metadata
SET agent_capabilities = '{"prompt_capabilities":{"image":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'ca45e378' AND agent_capabilities IS NULL;  -- amp-acp
-- cortex-code advertises no mcp_capabilities object at all.

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"session_capabilities":{"list":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'd5fd9849' AND agent_capabilities IS NULL;  -- cortex-code

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":false,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":false},"mcp_capabilities":{"http":false,"sse":false},"session_capabilities":{}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'e9bf0ab3' AND agent_capabilities IS NULL;  -- corust-agent

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":false,"sse":false},"session_capabilities":{"list":{},"additional_directories":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '5792d298' AND agent_capabilities IS NULL;  -- devin

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"mcp_capabilities":{"http":true,"sse":true},"prompt_capabilities":{"audio":true,"embedded_context":true,"image":true},"session":{"inject":{"modes":["queue","steer"],"pending":{"replace":true}},"inject_host_event":{"delivery":["turn_boundary","immediate","after_next_tool_call"],"kinds":["host_tool_result","host_attachment"]},"remind":{"modes":["interrupt_immediate","finish_step","audit_only"],"pending":{"list":true,"revoke":true}}},"session_capabilities":{"cancel_tool_call":{},"close":{},"list":{},"redo":{},"restore_tool_call":{},"resume":{},"rollback":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'ca2e896a' AND agent_capabilities IS NULL;  -- harn

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"audio":false,"image":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true},"session_capabilities":{"fork":{},"list":{},"resume":{},"additional_directories":{}},"auth":{"logout":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'edd2858a' AND agent_capabilities IS NULL;  -- junie
-- poolside advertises an EMPTY mcp_capabilities object (neither transport).

UPDATE agent_metadata
SET agent_capabilities = '{"auth":{"logout":{}},"load_session":true,"mcp_capabilities":{},"prompt_capabilities":{"image":true},"session_capabilities":{"close":{},"delete":{},"list":{}}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '0d1e478d' AND agent_capabilities IS NULL;  -- poolside

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":true,"audio":false,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":true},"session_capabilities":{}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'b6ba32f7' AND agent_capabilities IS NULL;  -- stakpak
-- vtcode gates its ACP surface behind VT_ACP_ENABLED, which its row already
-- carries in `env` (025); the probe had to pass the same variable explicitly.

UPDATE agent_metadata
SET agent_capabilities = '{"load_session":true,"prompt_capabilities":{"image":true,"audio":true,"embedded_context":true},"mcp_capabilities":{"http":true,"sse":false},"session_capabilities":{}}',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '2ab86949' AND agent_capabilities IS NULL;  -- vtcode

-- 4) Seed `auth_methods` from the same probe. Migration 003 pre-filled this
--    column alongside `agent_capabilities` for exactly the same reason — the UI
--    renders sign-in entry points from it — and 023 (pi) did too, but 025/029/031
--    omitted it, so 20 rows shipped unable to offer a login until the agent's
--    first successful start. `initialize` returns it without login, so it was
--    available at integration time.
--
--    Not seeded: cortex-code, dimcode, poolside and vtcode advertise no auth
--    methods at all; junie's advertise a JUNIE_HOME pointing at the probe host's
--    home directory, so its blob cannot be captured host-independently and is
--    left to the first real handshake. Per 003, a `_meta.terminal-auth.command`
--    emitted as an absolute path is stored as the bare command name (amp-acp).

UPDATE agent_metadata
SET auth_methods = '[{"id":"autohand-login","name":"Login to Autohand","description":"Sign in with your Autohand account","_meta":{"terminal-auth":{"command":"autohand","args":["login"],"label":"Autohand Login"}}}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'b3252207' AND auth_methods IS NULL;  -- autohand

UPDATE agent_metadata
SET auth_methods = '[{"id":"anthropic","name":"Anthropic API Key","type":"env_var","vars":[{"name":"ANTHROPIC_API_KEY"}],"link":"https://console.anthropic.com/settings/keys"},{"id":"openai","name":"OpenAI API Key","type":"env_var","vars":[{"name":"OPENAI_API_KEY"}],"link":"https://platform.openai.com/api-keys"},{"id":"deepagents-setup","name":"DeepAgents Setup","description":"Configure LLM provider credentials via environment variables"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'a11632d4' AND auth_methods IS NULL;  -- deepagents

UPDATE agent_metadata
SET auth_methods = '[{"id":"openai-codex-oauth","name":"Sign in with ChatGPT","description":"Authenticate with your ChatGPT Plus/Pro/Team subscription"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '6a95eb4f' AND auth_methods IS NULL;  -- dirac

UPDATE agent_metadata
SET auth_methods = '[{"id":"z-ai-api-key","name":"Z.AI API key","description":"Set Z_AI_API_KEY in the environment, or run `glm-acp-agent --setup` once to store the key on disk. Generate one at https://z.ai/manage-apikey/apikey-list"},{"type":"env_var","id":"z_ai_api_key","name":"Z.AI API key","description":"API key for the Z.AI / Zhipu AI service. Generate one at https://z.ai/manage-apikey/apikey-list","link":"https://z.ai/manage-apikey/apikey-list","vars":[{"name":"Z_AI_API_KEY","label":"Z.AI API key","secret":true,"optional":false}]}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '5431c523' AND auth_methods IS NULL;  -- glm-acp-agent

UPDATE agent_metadata
SET auth_methods = '[{"id":"grok.com","name":"Grok","description":"Sign in with Grok"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '145a4e5c' AND auth_methods IS NULL;  -- grok

UPDATE agent_metadata
SET auth_methods = '[{"description":"Run `kilo auth login` in the terminal","name":"Login with Kilo","id":"kilo-login"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '54c5ccf0' AND auth_methods IS NULL;  -- kilo

UPDATE agent_metadata
SET auth_methods = '[{"description":"Run `opencode auth login` in the terminal","name":"Login with opencode","id":"opencode-login"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '8f21c6d3' AND auth_methods IS NULL;  -- mimo-code

UPDATE agent_metadata
SET auth_methods = '[{"id":"kore-terminal-auth","type":"terminal","name":"Nova Setup","description":"Complete interactive setup in the terminal to configure API keys","args":["@compass-ai/nova","setup"],"env":{}}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '19e05df6' AND auth_methods IS NULL;  -- nova

UPDATE agent_metadata
SET auth_methods = '[{"id":"agent","name":"Use existing local credentials","description":"Authenticate via the provider keys/OAuth state already configured under ~/.omp."}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'c9e8a2f4' AND auth_methods IS NULL;  -- omp

UPDATE agent_metadata
SET auth_methods = '[{"id":"sigit","name":"Sign in to siGit Code","description":"Sign in with `/login <email> <password>` in the message box."}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '79767ac2' AND auth_methods IS NULL;  -- sigit

UPDATE agent_metadata
SET auth_methods = '[{"id":"setup","name":"Amp API Key Setup","description":"Run interactive setup to configure your Amp API key","_meta":{"terminal-auth":{"command":"amp-acp","args":["--setup"],"label":"Amp API Key Setup"}}}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'ca45e378' AND auth_methods IS NULL;  -- amp-acp

UPDATE agent_metadata
SET auth_methods = '[{"id":"oauth_browser","name":"Login via browser","description":"Open a browser and complete OAuth login"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'e9bf0ab3' AND auth_methods IS NULL;  -- corust-agent

UPDATE agent_metadata
SET auth_methods = '[{"id":"devin-browser","name":"Log in with browser","description":"Sign in via your browser"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = '5792d298' AND auth_methods IS NULL;  -- devin

UPDATE agent_metadata
SET auth_methods = '[{"_meta":{"harn":{"challenge":{"type":"none"},"scheme":"none"}},"description":"Connect without credentials. The agent runs locally and accepts the session as an anonymous principal.","id":"none","name":"Local (no authentication)","type":"agent"}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'ca2e896a' AND auth_methods IS NULL;  -- harn

UPDATE agent_metadata
SET auth_methods = '[{"id":"stakpak","name":"Login to Stakpak","description":"Authenticate via browser to get your Stakpak API key. A browser window will open for you to sign in."}]',
    updated_at = unixepoch('now','subsec')*1000
WHERE agent_id = 'b6ba32f7' AND auth_methods IS NULL;  -- stakpak
