CREATE TABLE project_repository_facts (
    project_id TEXT PRIMARY KEY NOT NULL,
    repository_url TEXT,
    default_branch TEXT,
    baseline_commit TEXT,
    repository_dirty INTEGER NOT NULL DEFAULT 0 CHECK (repository_dirty IN (0, 1)),
    dirty_worktree_choice TEXT NOT NULL CHECK (dirty_worktree_choice IN ('preserve', 'snapshot', 'reject')),
    dirty_snapshot_ref TEXT,
    credential_reference TEXT,
    detected_languages_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(detected_languages_json)),
    detected_package_managers_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(detected_package_managers_json)),
    detected_rules_files_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(detected_rules_files_json)),
    monorepo_packages_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(monorepo_packages_json)),
    submodules_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(submodules_json)),
    lfs_detected INTEGER NOT NULL DEFAULT 0 CHECK (lfs_detected IN (0, 1)),
    detected_at INTEGER NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

ALTER TABLE project_resource_links RENAME TO project_resource_links_legacy;

CREATE TABLE project_resource_links (
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    resource_type TEXT NOT NULL CHECK (resource_type IN ('conversation', 'team', 'cron', 'channel')),
    resource_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, user_id, resource_type, resource_id),
    UNIQUE (user_id, resource_type, resource_id),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

INSERT INTO project_resource_links (project_id, user_id, resource_type, resource_id, created_at)
SELECT project_id, user_id, resource_type, resource_id, created_at FROM project_resource_links_legacy;

DROP TABLE project_resource_links_legacy;

CREATE INDEX idx_project_resource_links_project
    ON project_resource_links(project_id, resource_type, created_at DESC);
