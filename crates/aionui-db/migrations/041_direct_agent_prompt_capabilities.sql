-- Prompt capabilities for the direct-CLI agents (feature: multimodal prompt).
--
-- ACP agents carry `prompt_capabilities` inside the handshake-persisted
-- `agent_metadata.agent_capabilities` (seeded by 003, refreshed live on every
-- initialize), and the column is passed to the frontend verbatim — so the UI
-- can already tell which ACP agents take native image/audio blocks. claude and
-- codex are routed as direct CLI connections and never handshake over ACP, so
-- their rows need the same key constructed here (exactly like 036 did for
-- `session_capabilities.fork`):
--
--   * claude 2.1.x: native base64 image input block, no audio
--     (adapter capability declaration; image frame pinned at
--     protocols/samples/claude-cli/2.1.177/image_input_frame.OK.json).
--   * codex 0.14x app-server: image input via turn input items, no audio.
--   * antigravity (`agy -p`) is text-only — no key, absent reads as
--     unsupported, which is correct.
--
-- `json_patch` merges into any pre-existing JSON instead of clobbering the
-- column. Rows are addressed by their stable seed ids (001), never by display
-- fields.

-- Claude Code (2d23ff1c)
UPDATE agent_metadata SET
    agent_capabilities = json_patch(
        COALESCE(agent_capabilities, '{}'),
        '{"prompt_capabilities":{"image":true,"audio":false}}'
    ),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '2d23ff1c';

-- Codex CLI (8e1acf31)
UPDATE agent_metadata SET
    agent_capabilities = json_patch(
        COALESCE(agent_capabilities, '{}'),
        '{"prompt_capabilities":{"image":true,"audio":false}}'
    ),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '8e1acf31';
