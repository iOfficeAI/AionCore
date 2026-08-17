-- Retire the DeepSeek Harness Preview catalog entry.
-- Conversations that already reference this agent stay on disk; send/create
-- paths treat the backend as archived. Do not DELETE the metadata row.
UPDATE agent_metadata
SET enabled = 0,
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_id = 'd54a7e91'
   OR backend = 'deepseek-harness';
