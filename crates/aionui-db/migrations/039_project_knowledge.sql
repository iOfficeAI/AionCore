CREATE TABLE project_knowledge_indexes (
    project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    provider_project_name TEXT NOT NULL,
    provider_version TEXT,
    status TEXT NOT NULL CHECK(status IN ('healthy', 'stale', 'indexing', 'failed', 'unavailable')),
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    source_commit TEXT,
    indexed_at INTEGER,
    changed_paths_json TEXT NOT NULL DEFAULT '[]',
    error_category TEXT,
    updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_project_knowledge_provider_name
    ON project_knowledge_indexes(provider, provider_project_name);
CREATE INDEX idx_project_knowledge_status
    ON project_knowledge_indexes(status, updated_at);

CREATE TABLE project_knowledge_facts (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK(generation > 0),
    kind TEXT NOT NULL CHECK(kind IN ('symbol', 'caller', 'test', 'route', 'data_entity', 'architecture')),
    name TEXT NOT NULL,
    qualified_name TEXT,
    source_path TEXT NOT NULL,
    source_line INTEGER CHECK(source_line IS NULL OR source_line > 0),
    indexed_at INTEGER NOT NULL
);

CREATE INDEX idx_project_knowledge_facts_generation
    ON project_knowledge_facts(project_id, generation, kind, name);

CREATE TABLE project_knowledge_contexts (
    id TEXT PRIMARY KEY NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider_project_name TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    query TEXT NOT NULL,
    symbols_json TEXT NOT NULL DEFAULT '[]',
    callers_json TEXT NOT NULL DEFAULT '[]',
    tests_json TEXT NOT NULL DEFAULT '[]',
    routes_json TEXT NOT NULL DEFAULT '[]',
    data_entities_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_project_knowledge_contexts_project
    ON project_knowledge_contexts(project_id, created_at DESC);
