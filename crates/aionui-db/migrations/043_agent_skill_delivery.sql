-- Per-vendor skill delivery declaration.
--
-- Deliberately NO CHECK constraint. Unlike `status IN (...)` style columns
-- (closed state machines, where a new value SHOULD be a conscious schema
-- change), this column EXISTS so a new vendor capability can ship as data.
-- A CHECK would (1) turn "add a mode" back into "write a migration", and
-- (2) hard-fail a registry insert carrying a newer mode on an older DB --
-- converting a degradable problem into an outage. Value validation lives in
-- the application layer and is deliberately tolerant
-- (`aionui-api-types/src/skill_delivery.rs`): an unknown mode warns with the
-- actual value and falls back to `injected`.
--
-- Rows are addressed by `backend`, the label the runtime keys on, rather than
-- by seed id (spread across 001 / 003 / 034).
--
-- Modes:
--   argv      launch-argument delivery (the CLI reads the view directory)
--   protocol  protocol-request delivery
--   injected  prompt injection + dual channel; the safe default, and what a
--             NULL column reads as
ALTER TABLE agent_metadata ADD COLUMN skill_delivery TEXT;

-- Layer 1 (argv), claude. Verified against claude 2.1.231:
--   * `claude --help` documents `--plugin-dir <path>` as repeatable
--     ("--plugin-dir A --plugin-dir B.zip") and `--add-dir <directories...>`.
--   * `claude --plugin-dir <root> plugin details` accepted the
--     `.claude-plugin/plugin.json` + `skills/{name}/SKILL.md` layout and
--     reported `Skills (1)`, including when the skill directory was a symlink.
--   * REPEATED `--add-dir` was probed as a matched pair under AionUi's actual
--     default permission mode (`default` -- `claude_conn.rs` falls back to it
--     when `config.mode` is empty, and `--allow-dangerously-skip-permissions`
--     only makes bypass REACHABLE, it does not change the initial mode):
--       - with two `--add-dir` flags, both out-of-cwd files were read;
--       - with none, both were refused as "outside the allowed working
--         directories".
--     The pair is what attributes success to the flag rather than to a lax mode.
--   * Registering a skill through `--plugin-dir` does NOT exempt it from that
--     path check, which is why layer 1 still needs `allow_dir_args`.
--
-- Allow-listing targets the REAL source dirs rather than the view, because a
-- CLI that resolves symlinks to their canonical path would not match a
-- view-directory entry.
UPDATE agent_metadata SET
    skill_delivery = '{"mode":"argv","args":["--plugin-dir","{skill_view_dir}"],"allow_dir_args":["--add-dir","{skill_dir}"]}',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE backend = 'claude';

-- codebuddy stays on layer 2 for now, deliberately.
--
-- What IS verified, against the version we actually spawn (`npx --package
-- @tencent-ai/codebuddy-code@2.138.0`, pinned in `registry_npx_lock.rs`; the
-- copy on a developer's PATH may be far older and lacks these flags):
--   * `--help` documents `--plugin-dir <dirs...>` ("Load plugins from local
--     directories (for development/testing)") and `--add-dir <directories...>`.
--   * argv PARSING accepts `--add-dir A --add-dir B --plugin-dir C` -- the run
--     proceeded to "Authentication required", i.e. it failed on auth, not flags.
--
-- What is NOT verified: whether `--plugin-dir` actually makes the skills
-- discoverable, which needs an authenticated account we do not have here.
-- Declaring `argv` on that basis would be a real regression risk: `argv` mode
-- also switches injection to LIGHT, so if the flag were inert codebuddy would
-- lose skills entirely. `injected` keeps it working through the dual channel,
-- and promoting it after a live probe is a ONE-ROW data change with no code
-- change and no release -- which is the whole reason this column exists.
UPDATE agent_metadata SET
    skill_delivery = '{"mode":"injected","allow_dir_args":["--add-dir","{skill_dir}"]}',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE backend = 'codebuddy';

-- Layer 1 (protocol), codex. `extraRoots` expects the SKILLS ROOT (directly
-- holding `{name}/SKILL.md`), not a plugin tree (verified: codex-cli 0.146.0
-- self-generated schema `v2/SkillsExtraRootsSetParams.json`; a live
-- `codex app-server --stdio` probe answered `{"result":{}}` and then pushed a
-- `skills/changed` notification, reporting the skill with `scope: "user"` and
-- the symlink resolved to its real source path).
--
-- That `scope: "user"` is a HARD CONSTRAINT on the consumer side: the request
-- is process-scoped, so codex layer-1 delivery is mutually exclusive with
-- thread multiplexing. See the enforcing comment in `codex_conn.rs`.
--
-- No `allow_dir_args`, and that is measured rather than assumed. A full
-- app-server turn probe (initialize -> extraRoots/set -> thread/start ->
-- turn/start) had the agent read a SYMLINKED skill's `references/` file living
-- outside the thread cwd, under `sandbox: workspace-write`, and it succeeded --
-- reaching the real source path. So codex needs no directory allow-listing for
-- reads, unlike claude.
--
-- Caveat worth knowing: that probe ran with `approvalPolicy: "never"`. Under
-- the `on-request` default an out-of-cwd read may surface an approval request
-- instead of being refused outright, which is the same "the user confirms"
-- behaviour claude has and which AionUi already renders a permission card for.
UPDATE agent_metadata SET
    skill_delivery = '{"mode":"protocol","method":"skills/extraRoots/set"}',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE backend = 'codex';

-- Layer 2. No session-scoped skill injection parameter exists for these
-- (verified against agy 1.1.13: `agy --help` offers no plugin/skill root flag,
-- and `agy plugin --help` is install/uninstall/enable/disable -- a persistent
-- install surface, not a session one; `opencode debug config` shows no external
-- skill root among its resolved top-level keys).
--
-- agy gets NO allow-listing, measured rather than assumed. It does take a
-- repeatable `--add-dir`, but our argv passes `--dangerously-skip-permissions`
-- unconditionally (`antigravity/argv.rs`), which puts the session in
-- `permission_mode: "always-proceed"`. A live probe under exactly that argv read
-- a file well outside the cwd with the containing directory NOT allow-listed, so
-- the flag would add one argument per skill for no effect.
--
-- Worse than useless: a config that lists allow_dir_args reads as "agy is
-- allow-listed", which is a claim this measurement contradicts.
--
-- If agy ever gains a real permission mode we stop bypassing, allow-listing
-- becomes necessary and this is a one-row change.
UPDATE agent_metadata SET
    skill_delivery = '{"mode":"injected","allow_dir_args":[]}',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE backend = 'antigravity';

UPDATE agent_metadata SET
    skill_delivery = '{"mode":"injected"}',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE backend = 'opencode';

-- Every other row stays NULL, which the application reads as `injected`. That
-- is the point: an unprobed vendor is zero-intrusion by default and needs no
-- migration to work.
