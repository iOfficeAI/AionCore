-- Keep the stable internal runtime identity (`aionrs`, agent id 632f31d2)
-- while replacing the legacy product name exposed through agent and assistant APIs.
UPDATE agent_metadata
SET name = 'CSBU WorkMate',
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE agent_id = '632f31d2'
  AND agent_type = 'aionrs'
  AND agent_source = 'internal'
  AND name IN ('Aion CLI', 'Aion Assistant', 'AionUi', 'AionUI', 'Aion UI', 'CSBU CLI');

-- Generated assistant definitions cache the agent display name. Updating them
-- here prevents an old name from leaking before startup reconciliation runs.
UPDATE assistant_definitions
SET name = 'CSBU WorkMate',
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE source = 'generated'
  AND agent_id = '632f31d2'
  AND name IN ('Aion CLI', 'Aion Assistant', 'AionUi', 'AionUI', 'Aion UI', 'CSBU CLI');
