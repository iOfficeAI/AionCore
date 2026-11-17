-- Migration 044: rebrand the internal CLI agent display name in existing
-- databases.
--
-- The SQL seed in migration 001 ships with the upstream name on purpose: sqlx
-- checksums the raw migration bytes (comments included) and aborts startup
-- with VersionMismatch when an already-applied migration's bytes change, so
-- the visible label is rebranded here — in data, via a new forward migration —
-- instead of editing shipped history. See reconcile_applied_migration_checksums
-- for the line-ending/comment drift this pattern avoids.
--
-- Only the display name is touched: id, backend and agent_type stay stable so
-- existing references (is_internal_aion_cli checks, overrides, skills routing)
-- keep resolving.
UPDATE agent_metadata
SET name = 'Super Digital CLI',
    updated_at = CAST(strftime('%s','now') AS INTEGER) * 1000
WHERE id = '632f31d2'
  AND name <> 'Super Digital CLI';
