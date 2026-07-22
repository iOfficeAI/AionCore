-- Telegram forum topics: one fixed Agent per topic.
ALTER TABLE assistant_sessions ADD COLUMN message_thread_id INTEGER;
ALTER TABLE assistant_sessions ADD COLUMN bound_agent_id TEXT;
ALTER TABLE assistant_sessions ADD COLUMN bound_backend TEXT;
ALTER TABLE assistant_sessions ADD COLUMN bound_provider_id TEXT;
ALTER TABLE assistant_sessions ADD COLUMN bound_model TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_assistant_sessions_user_chat_thread
  ON assistant_sessions(user_id, chat_id, message_thread_id);

CREATE TABLE IF NOT EXISTS telegram_topic_bindings (
  chat_id TEXT NOT NULL,
  message_thread_id INTEGER NOT NULL,
  agent_id TEXT NOT NULL,
  bound_by_user_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (chat_id, message_thread_id)
);

CREATE TABLE IF NOT EXISTS channel_topic_model_overrides (
  platform TEXT NOT NULL,
  internal_user_id TEXT NOT NULL,
  chat_id TEXT NOT NULL,
  message_thread_id INTEGER NOT NULL,
  agent_id TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  model TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (platform, internal_user_id, chat_id, message_thread_id)
);
