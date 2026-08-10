-- Conversation fork groundwork (feature: fork-as-new-conversation).
--
-- Two independent pieces, shipped together because both are prerequisites for
-- the fork API and neither changes runtime behavior on its own:
--
-- 1) `messages.backend_turn_id` — the backend-side turn anchor. codex's
--    `thread/fork` accepts an optional `lastTurnId` (fork at a specific turn,
--    dropping later history). Our runtime `turn_<shortid>` ids are minted by
--    the conversation layer and have NO relation to codex's internal Turn.id,
--    so the codex reader will start emitting the backend turn id when a turn
--    starts and the stream persistence layer stamps it onto every message row
--    it writes for that turn. Fork-point resolution is then a pure DB lookup
--    ("nearest non-NULL at or before the fork point"). Rows written before
--    this column existed stay NULL: HEAD forks never need the anchor
--    (`lastTurnId` omitted = fork at HEAD), and a mid-history fork request
--    that cannot resolve an anchor is rejected explicitly rather than
--    silently degraded to a HEAD fork. claude / ACP backends never emit the
--    event, so their rows remain NULL by design.
--
-- 2) Constructed fork capability for the two direct-CLI builtin agents.
--    ACP vendors advertise `sessionCapabilities.fork` in the `initialize`
--    handshake, which `registry::apply_handshake` persists into
--    `agent_metadata.agent_capabilities` (snake_cased, see
--    `normalize_keys_to_snake_case` and the pre-seeded shapes in migrations
--    003/033). claude and codex are routed as direct CLI connections and
--    never perform an ACP handshake, so their column stays NULL forever —
--    which is exactly why a constructed value here is safe: nothing will
--    overwrite it. Writing the SAME column in the SAME shape lets every
--    consumer (fork API validation, `ConversationResponse.fork_capability`
--    projection) read one uniform source regardless of backend kind.
--    CLI ground truth, verified 2026-08-04:
--      * codex 0.145.x app-server: `thread/fork {threadId, lastTurnId?}` —
--        supports forking at an arbitrary turn, hence `"at_turn": true`
--        (an aionui extension key inside the fork object; the ACP RFD
--        reserves the fork object for exactly this kind of extension).
--      * claude 2.1.x: `--resume <sid> --fork-session` — HEAD fork only,
--        hence an empty fork object.
--    Antigravity (agy) deliberately gets NO fork key: per product decision it
--    is not supported (its only fork surface is an internal RPC / TUI
--    command, not reachable from headless spawns).
--
-- `json_patch` merges into any pre-existing JSON instead of clobbering the
-- column (defensive: historical bridge builds may have written other keys).
-- Rows are addressed by their stable seed ids (001), never by display fields.

ALTER TABLE messages ADD COLUMN backend_turn_id TEXT;

-- Claude Code (2d23ff1c): HEAD-only fork via `--resume <sid> --fork-session`.
UPDATE agent_metadata SET
    agent_capabilities = json_patch(
        COALESCE(agent_capabilities, '{}'),
        '{"session_capabilities":{"fork":{}}}'
    ),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '2d23ff1c';

-- Codex CLI (8e1acf31): app-server `thread/fork` with optional `lastTurnId`,
-- i.e. fork at an arbitrary turn.
UPDATE agent_metadata SET
    agent_capabilities = json_patch(
        COALESCE(agent_capabilities, '{}'),
        '{"session_capabilities":{"fork":{"at_turn":true}}}'
    ),
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '8e1acf31';
