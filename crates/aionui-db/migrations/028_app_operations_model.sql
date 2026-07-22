ALTER TABLE system_settings
ADD COLUMN app_operations_model_mode TEXT NOT NULL DEFAULT 'auto'
CHECK (app_operations_model_mode IN ('auto', 'fixed'));

ALTER TABLE system_settings
ADD COLUMN app_operations_provider_id TEXT;

ALTER TABLE system_settings
ADD COLUMN app_operations_model_id TEXT;
