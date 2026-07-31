-- Antigravity (agy CLI) builtin agent.
--
-- Direct-CLI backend, NOT an ACP vendor: agy does not speak ACP, and unlike the
-- bridged rows it needs no npx/bun wrapper. `args` stays empty because agy's
-- argv is built per turn (-p <prompt> / --conversation / --add-dir), so nothing
-- about it is static.
--
-- yolo_id is NULL on purpose: agy's full-auto is the
-- `--dangerously-skip-permissions` FLAG, not a mode id. Its mode axis is
-- default / accept-edits / plan, and AgentType::full_auto_mode_id returns
-- "default" for this type.
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
     NULL,
     '{"available_modes":[{"id":"default","name":"Default"},{"id":"accept-edits","name":"Accept Edits"},{"id":"plan","name":"Plan"}],"current_mode_id":"default"}',
     3140,
     unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000);
