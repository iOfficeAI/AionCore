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
-- fields on legacy mirrors, conversation extra, and acp_session.
ALTER TABLE assistant_definitions RENAME COLUMN agent_backend TO agent_id;
ALTER TABLE assistant_overlays RENAME COLUMN agent_backend_override TO agent_id_override;
ALTER TABLE conversation_assistant_snapshots RENAME COLUMN agent_backend TO agent_id;

DROP INDEX IF EXISTS idx_assistant_definitions_agent_backend;

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

CREATE INDEX IF NOT EXISTS idx_assistant_overlays_agent_id_override
    ON assistant_overlays(agent_id_override)
    WHERE agent_id_override IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_conversation_assistant_snapshots_agent_id
    ON conversation_assistant_snapshots(agent_id);
