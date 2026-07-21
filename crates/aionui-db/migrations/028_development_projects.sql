CREATE TABLE projects (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    local_path TEXT NOT NULL,
    repository_url TEXT,
    default_branch TEXT,
    project_type TEXT NOT NULL DEFAULT 'unknown'
        CHECK (project_type IN ('single', 'monorepo', 'unknown')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE (user_id, local_path)
);

CREATE INDEX idx_projects_user_updated
    ON projects(user_id, updated_at DESC);

CREATE TABLE project_command_profiles (
    project_id TEXT PRIMARY KEY NOT NULL,
    install_command TEXT,
    format_command TEXT,
    lint_command TEXT,
    typecheck_command TEXT,
    unit_test_command TEXT,
    integration_test_command TEXT,
    e2e_command TEXT,
    build_command TEXT,
    security_scan_command TEXT,
    command_timeout_seconds INTEGER NOT NULL DEFAULT 900
        CHECK (command_timeout_seconds > 0),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE project_runtime_profiles (
    project_id TEXT PRIMARY KEY NOT NULL,
    environment_kind TEXT NOT NULL DEFAULT 'local'
        CHECK (environment_kind IN ('local', 'container')),
    language TEXT,
    package_manager TEXT,
    runtime_version TEXT,
    env_keys TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(env_keys)),
    metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE
);

CREATE TABLE project_resource_links (
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    resource_type TEXT NOT NULL CHECK (resource_type IN ('conversation', 'team')),
    resource_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, user_id, resource_type, resource_id),
    UNIQUE (user_id, resource_type, resource_id),
    FOREIGN KEY (project_id) REFERENCES projects(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE INDEX idx_project_resource_links_project
    ON project_resource_links(project_id, resource_type, created_at DESC);
