-- Rename the builtin aionrs engine display name. Do not change agent_id or agent_type.
UPDATE agent_metadata
SET name = 'Wework Agent',
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_id = '632f31d2'
  AND name = 'Aion CLI';
