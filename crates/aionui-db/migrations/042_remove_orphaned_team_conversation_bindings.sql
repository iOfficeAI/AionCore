-- Remove team-only conversation metadata left behind when a team was deleted
-- without unbinding its preserved origin conversation.
--
-- A row is repaired only when none of its non-empty team markers references a
-- currently existing team. Valid formal and ad-hoc team bindings are retained.
UPDATE conversations
SET extra = json_remove(
    extra,
    '$.teamId',
    '$.team_id',
    '$.slot_id',
    '$.role',
    '$.team_mcp_stdio_config'
)
WHERE json_valid(extra)
  AND (
      NULLIF(json_extract(extra, '$.teamId'), '') IS NOT NULL
      OR NULLIF(json_extract(extra, '$.team_id'), '') IS NOT NULL
  )
  AND NOT EXISTS (
      SELECT 1
      FROM teams
      WHERE teams.id = NULLIF(json_extract(conversations.extra, '$.teamId'), '')
         OR teams.id = NULLIF(json_extract(conversations.extra, '$.team_id'), '')
  );
