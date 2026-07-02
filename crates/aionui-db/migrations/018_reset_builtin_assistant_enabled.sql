-- One-time reset of built-in (official) assistant enabled state to the new
-- initial experience: only the AionUi butler is enabled by default; every
-- other official assistant is disabled so they no longer crowd the user's
-- selection lists.
--
-- Scope & safety:
--   * Touches ONLY existing overlay rows for `builtin` definitions.
--   * Writes ONLY the `enabled` column (+ updated_at). It never touches
--     `sort_order` (official order is manifest-owned) and never touches
--     `agent_backend_override` (the user's per-assistant engine/CLI choice).
--   * Built-in definitions WITHOUT an overlay row need no action here: the
--     application already falls back to the manifest default_enabled for them.
--   * Runs exactly once (sqlx migration). After this, any manual enable/disable
--     the user makes is a normal overlay write and is never reset again.
--
-- The butler is identified by its manifest source_ref 'aionui-assistant'.
--
-- Two tables must be reset in lockstep: the unified `assistant_overlays`
-- (current) and the legacy `assistant_overrides` (kept for backward-compat and
-- re-synced into overlays on every startup via sync_legacy_overrides_to_new_states).
-- Resetting only one lets the other clobber it on the next boot.
UPDATE assistant_overlays
SET enabled = CASE
        WHEN assistant_definition_id IN (
            SELECT id FROM assistant_definitions
            WHERE source = 'builtin' AND source_ref = 'aionui-assistant'
        ) THEN 1
        ELSE 0
    END,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE assistant_definition_id IN (
    SELECT id FROM assistant_definitions WHERE source = 'builtin'
);

-- Legacy mirror. Its primary key is the assistant_id (== builtin source_ref).
UPDATE assistant_overrides
SET enabled = CASE WHEN assistant_id = 'aionui-assistant' THEN 1 ELSE 0 END,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE assistant_id IN (
    SELECT assistant_id FROM assistant_definitions WHERE source = 'builtin'
);
