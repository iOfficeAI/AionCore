-- Make legacy user-authored assistant ids tenant-local. The unified
-- assistant_definitions table is already scoped by (user_id, assistant_id),
-- but this compatibility table retained its original global PRIMARY KEY(id),
-- allowing one account to reserve another account's id.

CREATE TABLE assistants_user_scoped (
    id                      TEXT NOT NULL,
    user_id                 TEXT NOT NULL DEFAULT 'system_default_user' REFERENCES users(id),
    name                    TEXT NOT NULL,
    description             TEXT,
    avatar                  TEXT,
    enabled_skills          TEXT,
    custom_skill_names      TEXT,
    disabled_builtin_skills TEXT,
    prompts                 TEXT,
    models                  TEXT,
    name_i18n               TEXT,
    description_i18n        TEXT,
    prompts_i18n            TEXT,
    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL,
    PRIMARY KEY (user_id, id)
);

INSERT INTO assistants_user_scoped (
    id, user_id, name, description, avatar, enabled_skills,
    custom_skill_names, disabled_builtin_skills, prompts, models,
    name_i18n, description_i18n, prompts_i18n, created_at, updated_at
)
SELECT
    id, COALESCE(user_id, 'system_default_user'), name, description, avatar, enabled_skills,
    custom_skill_names, disabled_builtin_skills, prompts, models,
    name_i18n, description_i18n, prompts_i18n, created_at, updated_at
FROM assistants;

DROP TABLE assistants;
ALTER TABLE assistants_user_scoped RENAME TO assistants;

CREATE INDEX idx_assistants_updated_at
    ON assistants(updated_at DESC);
CREATE INDEX idx_assistants_user_updated_at
    ON assistants(user_id, updated_at DESC);
