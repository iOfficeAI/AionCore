-- Cross-session messaging master switch (per user).
--
-- Default 1: the feature ships on, and the setting is an opt-out panic button
-- (spec §5.7 / §6.9.2). NOT NULL DEFAULT means existing rows get the enabled
-- state without a backfill pass.
ALTER TABLE system_settings
ADD COLUMN cross_session_message_enabled INTEGER NOT NULL DEFAULT 1;
