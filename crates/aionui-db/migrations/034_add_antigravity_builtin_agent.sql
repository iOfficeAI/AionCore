-- Antigravity (agy CLI) builtin agent.
--
-- Direct-CLI backend, NOT an ACP vendor: agy does not speak ACP, and unlike the
-- bridged rows it needs no npx/bun wrapper. `args` stays empty because agy's
-- argv is built per turn (-p <prompt> / --conversation / --add-dir), so nothing
-- about it is static.
--
-- `yolo_id` is the SENTINEL mode id `yolo`, not one of agy's real modes.
-- agy has no full-auto MODE: measured against 1.1.9, all three of its modes
-- (default / accept-edits / plan) are refused the "command" permission without
-- `--dangerously-skip-permissions` and all three run commands with it, so the
-- flag alone decides. `accept-edits` only auto-approves FILE EDITS (agy's own
-- wording) and `plan` is "research & plan only" — neither is full auto.
--
-- The sentinel exists so callers that mean "run unattended" (a teammate via
-- `session_mode_for_backend`, a scheduled run via `fallback_full_auto_mode`)
-- express it through the SAME channel every other agent uses, instead of the
-- backend sniffing where the session came from. It is also offered as a mode so
-- a user can choose full auto deliberately. The backend answers it by not
-- installing its approval hook, and never forwards it as agy's `--mode`.
--
-- behavior_policy deliberately omits `supports_team`: migration 033 retired the
-- `supports_team: false` form (it reads like a denial but is a no-op inside an
-- OR), and team capability is DERIVED from the backend + probed capabilities.
--
-- available_modes uses the same shape the capability projection writes back
-- (a top-level `available_modes` key plus `current_mode_id`), so the mode
-- picker is populated before the first session ever runs.
-- NOTE: `agent_id` is NOT NULL (added by 030) and carries the logical agent
-- identity; builtin rows mirror their `id` into it. Omitting it makes the whole
-- INSERT fail the NOT NULL constraint, and `OR IGNORE` swallows that silently —
-- the migration reports success while seeding nothing.
INSERT OR IGNORE INTO agent_metadata
    (id, agent_id, icon, name, description, backend, agent_type, agent_source, agent_source_info,
     enabled, command, args, env, native_skills_dirs, behavior_policy, yolo_id,
     available_modes, sort_order, created_at, updated_at)
VALUES
    ('a9f3c21e', 'a9f3c21e', '/api/assets/logos/ai-major/antigravity.svg', 'Antigravity',
     'Google Antigravity via the agy CLI',
     'antigravity', 'antigravity', 'builtin', '{"binary_name":"agy"}',
     1, 'agy', '[]', '[]',
     '[".agents/skills"]',
     '{"supports_side_question":false}',
     'yolo',
     '{"available_modes":[{"id":"default","name":"Default"},{"id":"accept-edits","name":"Accept Edits"},{"id":"plan","name":"Plan"},{"id":"yolo","name":"Full Auto"}],"current_mode_id":"default"}',
     3140,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);

-- `INSERT OR IGNORE` leaves an already-seeded row untouched, so a database that
-- applied an earlier revision of this migration would keep the old `yolo_id`
-- and mode list. Restate them for that row.
UPDATE agent_metadata
   SET yolo_id = 'yolo',
       available_modes = '{"available_modes":[{"id":"default","name":"Default"},{"id":"accept-edits","name":"Accept Edits"},{"id":"plan","name":"Plan"},{"id":"yolo","name":"Full Auto"}],"current_mode_id":"default"}'
 WHERE id = 'a9f3c21e';
