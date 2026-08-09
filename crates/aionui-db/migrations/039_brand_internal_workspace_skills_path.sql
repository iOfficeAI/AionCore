-- Keep the stable internal runtime identity while removing its legacy name
-- from newly provisioned user-visible workspace paths.
UPDATE agent_metadata
SET native_skills_dirs = '[".csbu-workmate/skills"]',
    updated_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE agent_id = '632f31d2'
  AND agent_type = 'aionrs'
  AND agent_source = 'internal'
  AND native_skills_dirs = '[".aionrs/skills"]';
