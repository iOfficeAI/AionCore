ALTER TABLE providers
ADD COLUMN model_settings TEXT NOT NULL DEFAULT '{}'
CHECK (json_valid(model_settings));
