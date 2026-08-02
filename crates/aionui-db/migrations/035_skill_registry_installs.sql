-- Track user-owned skills installed from the fixed CSBU SkillHub registry.
CREATE TABLE IF NOT EXISTS skill_registry_installs (
    id                 TEXT PRIMARY KEY NOT NULL,
    user_id            TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    skill_id           TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    registry_key       TEXT NOT NULL DEFAULT 'csbu-skillhub',
    namespace          TEXT NOT NULL,
    slug               TEXT NOT NULL,
    remote_skill_id    INTEGER NOT NULL,
    remote_version_id  INTEGER NOT NULL,
    installed_version  TEXT NOT NULL,
    installed_at       INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    UNIQUE(user_id, registry_key, namespace, slug),
    UNIQUE(user_id, skill_id)
);

CREATE INDEX IF NOT EXISTS idx_skill_registry_installs_user
    ON skill_registry_installs(user_id, updated_at DESC);
