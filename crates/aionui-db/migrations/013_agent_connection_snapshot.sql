-- Migration 013: persist the latest local-agent connection snapshot
--
-- Stores the most recent availability probe or session-feedback result on
-- `agent_metadata`. These columns are snapshots, not the live runtime truth.

ALTER TABLE agent_metadata ADD COLUMN last_check_status TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_kind TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_error_code TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_error_message TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_guidance TEXT;
ALTER TABLE agent_metadata ADD COLUMN last_check_latency_ms INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_check_at INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_success_at INTEGER;
ALTER TABLE agent_metadata ADD COLUMN last_failure_at INTEGER;

-- Self-repair overrides: user-supplied executable path and extra env vars,
-- layered on top of the seed row at projection time. Stored plaintext, same
-- as the existing `env` column.
ALTER TABLE agent_metadata ADD COLUMN command_override TEXT;
ALTER TABLE agent_metadata ADD COLUMN env_override TEXT;

-- Assistant/agent unification: assistant storage now binds to the concrete
-- agent catalog row. Runtime backend labels remain compatibility/runtime
-- fields on legacy mirrors and conversation extra.
--
-- Assistant identity boundary:
-- - `assistant_definitions.id` is the internal definition row id.
-- - `assistant_definitions.assistant_id` is the stable assistant id exposed to
--   callers, conversations, teams, channels, and cron.
-- - foreign keys to the definition row use `assistant_definition_id`.
ALTER TABLE assistant_definitions RENAME COLUMN definition_id TO id;
ALTER TABLE assistant_definitions RENAME COLUMN assistant_key TO assistant_id;
ALTER TABLE assistant_overlays RENAME COLUMN definition_id TO assistant_definition_id;
ALTER TABLE assistant_preferences RENAME COLUMN definition_id TO assistant_definition_id;
ALTER TABLE conversation_assistant_snapshots RENAME COLUMN assistant_key TO assistant_id;

ALTER TABLE assistant_definitions RENAME COLUMN agent_backend TO agent_id;
ALTER TABLE assistant_overlays RENAME COLUMN agent_backend_override TO agent_id_override;
ALTER TABLE conversation_assistant_snapshots RENAME COLUMN agent_backend TO agent_id;

DROP INDEX IF EXISTS idx_assistant_definitions_agent_backend;
DROP INDEX IF EXISTS idx_assistant_definitions_assistant_key;

UPDATE assistant_definitions
SET agent_id = COALESCE(
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE assistant_definitions.source = 'generated'
          AND assistant_definitions.source_ref IS NOT NULL
          AND am.id = assistant_definitions.source_ref
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.id = assistant_definitions.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.backend = assistant_definitions.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.agent_type = assistant_definitions.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    assistant_definitions.agent_id
);

UPDATE assistant_overlays
SET agent_id_override = COALESCE(
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.id = assistant_overlays.agent_id_override
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.backend = assistant_overlays.agent_id_override
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.agent_type = assistant_overlays.agent_id_override
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    assistant_overlays.agent_id_override
)
WHERE agent_id_override IS NOT NULL;

UPDATE conversation_assistant_snapshots
SET agent_id = COALESCE(
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.id = conversation_assistant_snapshots.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.backend = conversation_assistant_snapshots.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    (
        SELECT am.id
        FROM agent_metadata am
        WHERE am.agent_type = conversation_assistant_snapshots.agent_id
        ORDER BY
            CASE am.agent_source
                WHEN 'builtin' THEN 0
                WHEN 'internal' THEN 1
                ELSE 2
            END,
            am.sort_order ASC,
            am.name ASC
        LIMIT 1
    ),
    conversation_assistant_snapshots.agent_id
);

CREATE INDEX IF NOT EXISTS idx_assistant_definitions_agent_id
    ON assistant_definitions(agent_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_definitions_assistant_id
    ON assistant_definitions(assistant_id);

CREATE INDEX IF NOT EXISTS idx_assistant_overlays_agent_id_override
    ON assistant_overlays(agent_id_override)
    WHERE agent_id_override IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_conversation_assistant_snapshots_agent_id
    ON conversation_assistant_snapshots(agent_id);

-- ACP sessions persist only the concrete agent catalog row. Historical rows
-- may have only `agent_backend`; recover that into `agent_id` before dropping
-- the ambiguous runtime label column.
DROP INDEX IF EXISTS idx_acp_session_status;
DROP INDEX IF EXISTS idx_acp_session_suspended;
DROP INDEX IF EXISTS idx_acp_session_agent_id;

CREATE TABLE _acp_session_new (
    conversation_id TEXT PRIMARY KEY,
    agent_source    TEXT    NOT NULL,
    agent_id        TEXT    NOT NULL,
    session_id      TEXT,
    session_status  TEXT    NOT NULL DEFAULT 'idle',
    session_config  TEXT    NOT NULL DEFAULT '{}',
    last_active_at  INTEGER,
    suspended_at    INTEGER
);

INSERT INTO _acp_session_new (
    conversation_id, agent_source, agent_id, session_id, session_status,
    session_config, last_active_at, suspended_at
)
SELECT
    conversation_id,
    agent_source,
    COALESCE(
        NULLIF(agent_id, ''),
        (
            SELECT am.id
            FROM agent_metadata am
            WHERE am.id = acp_session.agent_backend
               OR am.backend = acp_session.agent_backend
               OR am.agent_type = acp_session.agent_backend
            ORDER BY
                CASE am.agent_source
                    WHEN 'builtin' THEN 0
                    ELSE 1
                END,
                am.sort_order,
                am.id
            LIMIT 1
        ),
        ''
    ),
    session_id,
    session_status,
    session_config,
    last_active_at,
    suspended_at
FROM acp_session;

DROP TABLE acp_session;
ALTER TABLE _acp_session_new RENAME TO acp_session;

CREATE INDEX IF NOT EXISTS idx_acp_session_status ON acp_session(session_status);
CREATE INDEX IF NOT EXISTS idx_acp_session_suspended ON acp_session(session_status, suspended_at) WHERE session_status = 'suspended';
CREATE INDEX IF NOT EXISTS idx_acp_session_agent_id ON acp_session(agent_id);

-- Drop legacy assistant runtime-backend mirror columns after their values have
-- already been normalized into assistant_definitions.agent_id /
-- assistant_overlays.agent_id_override above.
CREATE TABLE IF NOT EXISTS _assistants_new (
    id                      TEXT PRIMARY KEY,
    name                    TEXT NOT NULL,
    description             TEXT,
    avatar                  TEXT,
    enabled_skills          TEXT,
    custom_skill_names      TEXT,
    disabled_builtin_skills TEXT,
    prompts                 TEXT,
    models                  TEXT,
    name_i18n               TEXT,
    description_i18n        TEXT,
    prompts_i18n            TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);

INSERT OR IGNORE INTO _assistants_new
    (id, name, description, avatar, enabled_skills, custom_skill_names,
     disabled_builtin_skills, prompts, models, name_i18n, description_i18n,
     prompts_i18n, created_at, updated_at)
SELECT
    id, name, description, avatar, enabled_skills, custom_skill_names,
    disabled_builtin_skills, prompts, models, name_i18n, description_i18n,
    prompts_i18n, created_at, updated_at
FROM assistants;

ALTER TABLE assistants RENAME TO _assistants_old;
ALTER TABLE _assistants_new RENAME TO assistants;
DROP TABLE IF EXISTS _assistants_old;
CREATE INDEX IF NOT EXISTS idx_assistants_updated_at ON assistants(updated_at DESC);

CREATE TABLE IF NOT EXISTS _assistant_overrides_new (
    assistant_id TEXT PRIMARY KEY,
    enabled      INTEGER NOT NULL DEFAULT 1,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    last_used_at INTEGER,
    updated_at   INTEGER NOT NULL
);

INSERT OR IGNORE INTO _assistant_overrides_new
    (assistant_id, enabled, sort_order, last_used_at, updated_at)
SELECT assistant_id, enabled, sort_order, last_used_at, updated_at
FROM assistant_overrides;

ALTER TABLE assistant_overrides RENAME TO _assistant_overrides_old;
ALTER TABLE _assistant_overrides_new RENAME TO assistant_overrides;
DROP TABLE IF EXISTS _assistant_overrides_old;

-- Some migration-012 databases predate generated "bare" assistant materializing.
-- Cron rows in that shape can still point at an ACP agent through the
-- conversation/acp_session runtime identity. Materialize the referenced
-- generated assistant rows before cron agent_config is rewritten so those jobs
-- keep a resolvable assistant_id instead of being disabled during migration.
INSERT OR IGNORE INTO assistant_definitions (
    id,
    assistant_id,
    source,
    owner_type,
    source_ref,
    source_version,
    source_hash,
    name,
    name_i18n,
    description,
    description_i18n,
    avatar_type,
    avatar_value,
    agent_id,
    rule_resource_type,
    rule_resource_ref,
    rule_inline_content,
    recommended_prompts,
    recommended_prompts_i18n,
    default_model_mode,
    default_model_value,
    default_permission_mode,
    default_permission_value,
    default_skills_mode,
    default_skill_ids,
    custom_skill_names,
    default_disabled_builtin_skill_ids,
    default_mcps_mode,
    default_mcp_ids,
    created_at,
    updated_at,
    deleted_at
)
WITH referenced_agent_ids AS (
    SELECT DISTINCT agent_id
    FROM (
        SELECT NULLIF(TRIM(acp_session.agent_id), '') AS agent_id
        FROM cron_jobs
        JOIN acp_session ON acp_session.conversation_id = cron_jobs.conversation_id

        UNION

        SELECT NULLIF(TRIM(json_extract(conversations.extra, '$.agent_id')), '') AS agent_id
        FROM cron_jobs
        JOIN conversations ON conversations.id = cron_jobs.conversation_id
        WHERE json_valid(conversations.extra)

        UNION

        SELECT (
            SELECT am.id
            FROM agent_metadata am
            WHERE am.id = cron_jobs.agent_type
               OR am.backend = cron_jobs.agent_type
               OR (cron_jobs.agent_type != 'acp' AND am.agent_type = cron_jobs.agent_type)
            ORDER BY
                CASE am.agent_source
                    WHEN 'builtin' THEN 0
                    WHEN 'internal' THEN 1
                    ELSE 2
                END,
                am.sort_order ASC,
                am.name ASC
            LIMIT 1
        ) AS agent_id
        FROM cron_jobs
    )
    WHERE agent_id IS NOT NULL
)
SELECT
    'asstdef_generated_' || am.id,
    'bare:' || am.id,
    'generated',
    'system',
    am.id,
    NULL,
    NULL,
    am.name,
    '{}',
    am.description,
    '{}',
    CASE
        WHEN NULLIF(TRIM(COALESCE(am.icon, '')), '') IS NOT NULL THEN 'emoji'
        ELSE 'none'
    END,
    NULLIF(TRIM(COALESCE(am.icon, '')), ''),
    am.id,
    'none',
    NULL,
    NULL,
    '[]',
    '{}',
    'auto',
    NULL,
    'auto',
    NULL,
    'auto',
    '[]',
    '[]',
    '[]',
    'auto',
    '[]',
    am.created_at,
    am.updated_at,
    NULL
FROM agent_metadata am
JOIN referenced_agent_ids rai ON rai.agent_id = am.id
WHERE NOT EXISTS (
    SELECT 1
    FROM assistant_definitions ad
    WHERE ad.source = 'generated'
      AND ad.source_ref = am.id
);

-- Cron assistant-first cleanup:
-- - `cron_jobs.agent_type` is derived from assistant identity at runtime.
-- - `agent_config.backend` was overloaded. For aionrs rows it held the LLM
--   provider_id, so migrate it into `agent_config.model.provider_id`.
--   Runtime backend is no longer persisted in cron config.
UPDATE cron_jobs
SET agent_config = json_remove(
    CASE
        WHEN agent_type = 'aionrs'
             AND json_extract(agent_config, '$.backend') IS NOT NULL
             AND TRIM(json_extract(agent_config, '$.backend')) != ''
        THEN json_set(
            agent_config,
            '$.model',
            json_object(
                'provider_id', json_extract(agent_config, '$.backend'),
                'model', COALESCE(NULLIF(TRIM(json_extract(agent_config, '$.model_id')), ''), 'default'),
                'use_model', json_extract(agent_config, '$.model_id')
            )
        )
        ELSE agent_config
    END,
    '$.backend',
    '$.custom_agent_id',
    '$.preset_agent_type',
    '$.is_preset',
    '$.cli_path'
)
WHERE agent_config IS NOT NULL
  AND json_valid(agent_config);

UPDATE cron_jobs
SET agent_config = json_set(
    COALESCE(agent_config, '{}'),
    '$.assistant_id',
    COALESCE(
        NULLIF(TRIM(json_extract(agent_config, '$.assistant_id')), ''),
        (
            SELECT cas.assistant_id
            FROM conversation_assistant_snapshots cas
            WHERE cas.conversation_id = cron_jobs.conversation_id
            LIMIT 1
        ),
        (
            SELECT ad.assistant_id
            FROM assistant_definitions ad
            WHERE ad.deleted_at IS NULL
              AND ad.agent_id = COALESCE(
                  (
                      SELECT NULLIF(TRIM(s.agent_id), '')
                      FROM acp_session s
                      WHERE s.conversation_id = cron_jobs.conversation_id
                      LIMIT 1
                  ),
                  (
                      SELECT NULLIF(TRIM(json_extract(c.extra, '$.agent_id')), '')
                      FROM conversations c
                      WHERE c.id = cron_jobs.conversation_id
                        AND json_valid(c.extra)
                      LIMIT 1
                  ),
                  (
                      SELECT am.id
                      FROM agent_metadata am
                      WHERE am.id = cron_jobs.agent_type
                         OR am.backend = cron_jobs.agent_type
                         OR (cron_jobs.agent_type != 'acp' AND am.agent_type = cron_jobs.agent_type)
                      ORDER BY
                          CASE am.agent_source
                              WHEN 'builtin' THEN 0
                              WHEN 'internal' THEN 1
                              ELSE 2
                          END,
                          am.sort_order ASC,
                          am.name ASC
                      LIMIT 1
                  ),
                  cron_jobs.agent_type
              )
            ORDER BY
                CASE ad.source
                    WHEN 'builtin' THEN 0
                    WHEN 'generated' THEN 1
                    ELSE 2
                END,
                ad.name ASC
            LIMIT 1
        )
    )
)
WHERE (agent_config IS NULL OR json_valid(agent_config))
  AND COALESCE(NULLIF(TRIM(json_extract(agent_config, '$.assistant_id')), ''), '') = ''
  AND COALESCE(
      (
          SELECT cas.assistant_id
          FROM conversation_assistant_snapshots cas
          WHERE cas.conversation_id = cron_jobs.conversation_id
          LIMIT 1
      ),
      (
          SELECT ad.assistant_id
          FROM assistant_definitions ad
          WHERE ad.deleted_at IS NULL
            AND ad.agent_id = COALESCE(
                (
                    SELECT NULLIF(TRIM(s.agent_id), '')
                    FROM acp_session s
                    WHERE s.conversation_id = cron_jobs.conversation_id
                    LIMIT 1
                ),
                (
                    SELECT NULLIF(TRIM(json_extract(c.extra, '$.agent_id')), '')
                    FROM conversations c
                    WHERE c.id = cron_jobs.conversation_id
                      AND json_valid(c.extra)
                    LIMIT 1
                ),
                (
                    SELECT am.id
                    FROM agent_metadata am
                    WHERE am.id = cron_jobs.agent_type
                       OR am.backend = cron_jobs.agent_type
                       OR (cron_jobs.agent_type != 'acp' AND am.agent_type = cron_jobs.agent_type)
                    ORDER BY
                        CASE am.agent_source
                            WHEN 'builtin' THEN 0
                            WHEN 'internal' THEN 1
                            ELSE 2
                        END,
                        am.sort_order ASC,
                        am.name ASC
                    LIMIT 1
                ),
                cron_jobs.agent_type
            )
          ORDER BY
              CASE ad.source
                  WHEN 'builtin' THEN 0
                  WHEN 'generated' THEN 1
                  ELSE 2
              END,
              ad.name ASC
          LIMIT 1
      )
  ) IS NOT NULL;

UPDATE cron_jobs
SET enabled = 0,
    last_status = 'error',
    last_error = 'invalid agent_config JSON during cron assistant-first migration'
WHERE agent_config IS NOT NULL
  AND NOT json_valid(agent_config);

UPDATE cron_jobs
SET enabled = 0,
    last_status = 'error',
    last_error = 'assistant_id could not be recovered during cron assistant-first migration'
WHERE (agent_config IS NULL OR json_valid(agent_config))
  AND COALESCE(NULLIF(TRIM(json_extract(agent_config, '$.assistant_id')), ''), '') = '';

DROP INDEX IF EXISTS idx_cron_jobs_agent_type;

CREATE TABLE IF NOT EXISTS _cron_jobs_new (
    id                   TEXT    PRIMARY KEY NOT NULL,
    name                 TEXT    NOT NULL,
    enabled              INTEGER NOT NULL DEFAULT 1,
    schedule_kind        TEXT    NOT NULL CHECK(schedule_kind IN ('at', 'every', 'cron')),
    schedule_value       TEXT    NOT NULL,
    schedule_tz          TEXT,
    schedule_description TEXT,
    payload_message      TEXT    NOT NULL,
    execution_mode       TEXT    NOT NULL DEFAULT 'existing'
                                 CHECK(execution_mode IN ('existing', 'new_conversation')),
    agent_config         TEXT,
    conversation_id      TEXT    NOT NULL,
    conversation_title   TEXT,
    created_by           TEXT    NOT NULL CHECK(created_by IN ('user', 'agent')),
    skill_content        TEXT,
    description          TEXT,
    created_at           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    next_run_at          INTEGER,
    last_run_at          INTEGER,
    last_status          TEXT    CHECK(last_status IN ('ok', 'error', 'skipped', 'missed')),
    last_error           TEXT,
    run_count            INTEGER NOT NULL DEFAULT 0,
    retry_count          INTEGER NOT NULL DEFAULT 0,
    max_retries          INTEGER NOT NULL DEFAULT 3
);

INSERT OR IGNORE INTO _cron_jobs_new
    (id, name, enabled, schedule_kind, schedule_value, schedule_tz, schedule_description,
     payload_message, execution_mode, agent_config, conversation_id, conversation_title,
     created_by, skill_content, description, created_at, updated_at,
     next_run_at, last_run_at, last_status, last_error, run_count, retry_count, max_retries)
SELECT
    id, name, enabled, schedule_kind, schedule_value, schedule_tz, schedule_description,
    payload_message, execution_mode, agent_config, conversation_id, conversation_title,
    created_by, skill_content, description, created_at, updated_at,
    next_run_at, last_run_at, last_status, last_error, run_count, retry_count, max_retries
FROM cron_jobs;

ALTER TABLE cron_jobs RENAME TO _cron_jobs_old;
ALTER TABLE _cron_jobs_new RENAME TO cron_jobs;
DROP TABLE IF EXISTS _cron_jobs_old;

CREATE INDEX IF NOT EXISTS idx_cron_jobs_conversation ON cron_jobs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run_at) WHERE enabled = 1;
