------------------------------------------------------------------------
-- Team multi-session: a team may own multiple working sessions, each
-- with its own per-slot conversations, mailbox messages, and task board.
-- The team roster (backend / model / name / role) and `session_mode` stay
-- shared at the team level; only the conversational state is partitioned
-- by `session_id`.
--
-- Backward compatibility: every existing team gets exactly one "primary"
-- session row (`is_primary = 1`), and its historical mailbox / task rows
-- are backfilled onto that primary session. All callers that do not pass
-- a `session_id` keep operating on the primary session, so pre-feature
-- behaviour is preserved byte-for-byte.
--
-- Constraints intentionally omitted (per project convention, see 028):
--   - no FOREIGN KEY
--   - no CHECK constraint
-- Enum / flag semantics live in SQL comments and are validated in the
-- service layer (aionui-team). Business identities are TEXT; the INTEGER
-- AUTOINCREMENT `id` on team_session_agents is an internal-only surrogate.
--
-- `session_id` naming note: the unrelated `team_agent_bindings.session_id`
-- column is the ACP protocol session; this migration's `session_id` is the
-- team working-session. They never co-occur on the same table.
------------------------------------------------------------------------

-- teams: current active working session pointer. NULL on legacy rows is
-- resolved lazily to the primary session by the service layer, and is
-- backfilled below for every existing team so it is never NULL in practice.
ALTER TABLE teams ADD COLUMN active_session_id TEXT;

-- A working session owned by a team. `is_primary = 1` marks the one
-- session auto-created with the team; it cannot be deleted (service-layer
-- guard) and is the fallback target for session_id-less callers.
CREATE TABLE IF NOT EXISTS team_sessions (
    id         TEXT    PRIMARY KEY NOT NULL,   -- stable identity (UUID v7 in app; 'primary_<team_id>' for backfill)
    team_id    TEXT    NOT NULL,
    name       TEXT    NOT NULL,               -- user-visible name
    is_primary INTEGER NOT NULL DEFAULT 0,     -- 1 = auto-created default session, undeletable
    created_at INTEGER NOT NULL,               -- epoch ms
    updated_at INTEGER NOT NULL                -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_team_sessions_team_id ON team_sessions(team_id);
-- A team may have at most one primary session.
CREATE UNIQUE INDEX IF NOT EXISTS idx_team_sessions_one_primary ON team_sessions(team_id) WHERE is_primary = 1;
-- Session names are unique within a team.
CREATE UNIQUE INDEX IF NOT EXISTS idx_team_sessions_team_name_unique ON team_sessions(team_id, name);

-- Per-session, per-slot conversation binding. The team roster
-- (`teams.agents` JSON) is the configuration template; this table holds
-- the concrete conversation_id minted for each (session, slot) pair.
-- The primary session's rows mirror the conversation_ids already present
-- in `teams.agents` (backfilled below); secondary sessions get fresh ones.
CREATE TABLE IF NOT EXISTS team_session_agents (
    id              INTEGER PRIMARY KEY AUTOINCREMENT, -- internal surrogate, not a business identity
    session_id      TEXT    NOT NULL,
    team_id         TEXT    NOT NULL,
    slot_id         TEXT    NOT NULL,
    conversation_id TEXT    NOT NULL,
    created_at      INTEGER NOT NULL                          -- epoch ms
);

CREATE INDEX IF NOT EXISTS idx_team_session_agents_session ON team_session_agents(session_id);
CREATE INDEX IF NOT EXISTS idx_team_session_agents_team_slot ON team_session_agents(team_id, slot_id);
CREATE INDEX IF NOT EXISTS idx_team_session_agents_conversation ON team_session_agents(conversation_id);
-- One conversation per (session, slot).
CREATE UNIQUE INDEX IF NOT EXISTS idx_team_session_agents_session_slot_unique ON team_session_agents(session_id, slot_id);

-- mailbox / team_tasks gain a session dimension. Nullable: NULL marks
-- legacy rows the backfill below assigns to the primary session. After
-- backfill, no row should remain NULL, but the column stays nullable so
-- future schema evolution and tests can mint rows before session binding.
ALTER TABLE mailbox ADD COLUMN session_id TEXT;
ALTER TABLE team_tasks ADD COLUMN session_id TEXT;

CREATE INDEX IF NOT EXISTS idx_mailbox_team_session_to_read ON mailbox(team_id, session_id, to_agent_id, read);
CREATE INDEX IF NOT EXISTS idx_team_tasks_team_session ON team_tasks(team_id, session_id);

------------------------------------------------------------------------
-- Backfill: seed one primary session per existing team, bind its slots'
-- conversations into team_session_agents, point the team's
-- active_session_id at it, and assign legacy mailbox / task rows to it.
--
-- The primary session id is deterministic ('primary_' || team_id) so the
-- migration is idempotent: re-running the INSERT...WHERE NOT EXISTS and
-- UPDATEs is a no-op. New teams created after this migration get a
-- random UUID v7 id from the service layer instead.
------------------------------------------------------------------------

INSERT INTO team_sessions (id, team_id, name, is_primary, created_at, updated_at)
SELECT
    'primary_' || t.id,
    t.id,
    'Main',
    1,
    t.created_at,
    t.updated_at
FROM teams t
WHERE NOT EXISTS (
    SELECT 1 FROM team_sessions ts WHERE ts.team_id = t.id AND ts.is_primary = 1
);

-- Set the active session pointer for every team that does not yet have one.
UPDATE teams
SET active_session_id = 'primary_' || teams.id
WHERE active_session_id IS NULL
  AND teams.id IN (SELECT team_id FROM team_sessions WHERE is_primary = 1);

-- Backfill per-slot conversation bindings for primary sessions from the
-- roster JSON. `teams.agents` is a JSON array of objects each carrying a
-- `slot_id` and a `conversation_id`. sqlite's json_each walks the array;
-- the composite SELECT filters out pairs already bound (idempotent).
-- `json_valid(...)` guards against a corrupted roster row: a malformed
-- `agents` value skips this backfill for that team (its primary session
-- still exists; the service layer reconciles bindings lazily) instead of
-- aborting the whole migration and blocking app startup.
INSERT INTO team_session_agents (session_id, team_id, slot_id, conversation_id, created_at)
SELECT
    ts.id,
    ts.team_id,
    je.value ->> '$.slot_id'           AS slot_id,
    je.value ->> '$.conversation_id'   AS conversation_id,
    ts.created_at
FROM team_sessions ts
JOIN teams t ON t.id = ts.team_id
JOIN json_each(t.agents) AS je
WHERE ts.is_primary = 1
  AND json_valid(t.agents)
  AND je.value ->> '$.slot_id' IS NOT NULL
  AND je.value ->> '$.conversation_id' IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM team_session_agents tsa
      WHERE tsa.session_id = ts.id
        AND tsa.slot_id = je.value ->> '$.slot_id'
  );

-- Assign legacy mailbox rows to the owning team's primary session.
-- Rows that already carry a session_id (written by new code paths) are
-- left untouched.
UPDATE mailbox
SET session_id = 'primary_' || mailbox.team_id
WHERE session_id IS NULL
  AND mailbox.team_id IN (SELECT team_id FROM team_sessions WHERE is_primary = 1);

-- Same for legacy task rows.
UPDATE team_tasks
SET session_id = 'primary_' || team_tasks.team_id
WHERE session_id IS NULL
  AND team_tasks.team_id IN (SELECT team_id FROM team_sessions WHERE is_primary = 1);
