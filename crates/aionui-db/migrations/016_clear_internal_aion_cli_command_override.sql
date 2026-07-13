-- Internal Aion CLI is hosted in-process and must not inherit self-repair
-- executable overrides that are intended for external CLI rows.
UPDATE agent_metadata
SET command = NULL,
    command_override = NULL,
    env_override = NULL,
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE agent_type = 'aionrs'
  AND agent_source = 'internal'
  AND (command IS NOT NULL OR command_override IS NOT NULL OR env_override IS NOT NULL);
