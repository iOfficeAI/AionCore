-- Add explicit source-channel metadata for conversations and teams.
--
-- Older rows stored this mostly in `source`, `channel_chat_id`, or
-- `extra.source_channel`. New rows should persist first-class fields so UI
-- grouping and future channel integrations do not depend on JSON inference.

ALTER TABLE conversations ADD COLUMN source_channel TEXT;
ALTER TABLE conversations ADD COLUMN source_channel_id TEXT;
ALTER TABLE conversations ADD COLUMN source_chat_id TEXT;
ALTER TABLE conversations ADD COLUMN source_user_id TEXT;
ALTER TABLE conversations ADD COLUMN source_label TEXT;
ALTER TABLE conversations ADD COLUMN created_from TEXT;

UPDATE conversations
SET
    source_channel = COALESCE(
        NULLIF(json_extract(extra, '$.source_channel'), ''),
        CASE
            WHEN source = 'aionui' THEN 'webui'
            WHEN source IS NOT NULL AND source <> '' THEN source
            ELSE NULL
        END
    ),
    source_channel_id = NULLIF(json_extract(extra, '$.source_channel_id'), ''),
    source_chat_id = COALESCE(
        NULLIF(json_extract(extra, '$.source_chat_id'), ''),
        NULLIF(channel_chat_id, '')
    ),
    source_user_id = NULLIF(json_extract(extra, '$.source_user_id'), ''),
    source_label = COALESCE(
        NULLIF(json_extract(extra, '$.source_label'), ''),
        CASE
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) IN ('aionui', 'webui') THEN 'WebUI'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'telegram' THEN 'Telegram'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'discord' THEN 'Discord'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'lark' THEN 'Lark'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'wecom' THEN 'WeCom'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'weixin' THEN 'Weixin'
            WHEN COALESCE(NULLIF(json_extract(extra, '$.source_channel'), ''), source) = 'dingtalk' THEN 'DingTalk'
            ELSE NULL
        END
    ),
    created_from = COALESCE(
        NULLIF(json_extract(extra, '$.created_from'), ''),
        CASE
            WHEN source = 'aionui' THEN 'webui'
            WHEN source IS NOT NULL AND source <> '' THEN source
            ELSE NULL
        END
    )
WHERE source_channel IS NULL;

ALTER TABLE teams ADD COLUMN source_channel TEXT;
ALTER TABLE teams ADD COLUMN source_channel_id TEXT;
ALTER TABLE teams ADD COLUMN source_chat_id TEXT;
ALTER TABLE teams ADD COLUMN source_user_id TEXT;
ALTER TABLE teams ADD COLUMN source_label TEXT;
ALTER TABLE teams ADD COLUMN created_from TEXT;

UPDATE teams
SET
    source_channel = (
        SELECT COALESCE(
            c.source_channel,
            NULLIF(json_extract(c.extra, '$.source_channel'), ''),
            CASE
                WHEN c.source = 'aionui' THEN 'webui'
                WHEN c.source IS NOT NULL AND c.source <> '' THEN c.source
                ELSE NULL
            END
        )
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    ),
    source_channel_id = (
        SELECT COALESCE(c.source_channel_id, NULLIF(json_extract(c.extra, '$.source_channel_id'), ''))
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    ),
    source_chat_id = (
        SELECT COALESCE(c.source_chat_id, NULLIF(json_extract(c.extra, '$.source_chat_id'), ''), NULLIF(c.channel_chat_id, ''))
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    ),
    source_user_id = (
        SELECT COALESCE(c.source_user_id, NULLIF(json_extract(c.extra, '$.source_user_id'), ''))
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    ),
    source_label = (
        SELECT COALESCE(
            c.source_label,
            NULLIF(json_extract(c.extra, '$.source_label'), ''),
            CASE
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) IN ('aionui', 'webui') THEN 'WebUI'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'telegram' THEN 'Telegram'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'discord' THEN 'Discord'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'lark' THEN 'Lark'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'wecom' THEN 'WeCom'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'weixin' THEN 'Weixin'
                WHEN COALESCE(c.source_channel, NULLIF(json_extract(c.extra, '$.source_channel'), ''), c.source) = 'dingtalk' THEN 'DingTalk'
                ELSE NULL
            END
        )
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    ),
    created_from = (
        SELECT COALESCE(
            c.created_from,
            NULLIF(json_extract(c.extra, '$.created_from'), ''),
            CASE
                WHEN c.source = 'aionui' THEN 'webui'
                WHEN c.source IS NOT NULL AND c.source <> '' THEN c.source
                ELSE NULL
            END
        )
        FROM conversations c
        WHERE c.id = COALESCE(
            NULLIF(json_extract(teams.agents, '$[0].conversation_id'), ''),
            NULLIF(json_extract(teams.agents, '$[0].conversationId'), '')
        )
    )
WHERE source_channel IS NULL;

CREATE INDEX IF NOT EXISTS idx_conversations_source_channel ON conversations(source_channel);
CREATE INDEX IF NOT EXISTS idx_conversations_source_channel_chat ON conversations(source_channel, source_chat_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_teams_source_channel ON teams(source_channel);
