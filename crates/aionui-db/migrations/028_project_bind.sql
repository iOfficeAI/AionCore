------------------------------------------------------------------------
-- Project bind chain: Project / Folder / ProjectExplorer
--
-- Authoritative model for the Project Explorer creation-binding chain.
-- Constraints intentionally omitted (per design):
--   - no FOREIGN KEY
--   - no CHECK constraint
-- Enum semantics live in SQL comments and are validated in the service
-- layer (aionui-project). Business identities are UUID v7 TEXT columns;
-- the INTEGER AUTOINCREMENT `id` is an internal-only surrogate key.
------------------------------------------------------------------------

-- Project is a business container; it does NOT store a workspace string or
-- folder_id. Explorer roots live in project_explorer.
-- kind service-layer enum: standard / temp
--   standard: user-selected workspace resource root; locally a file: dir
--   temp:     system-created temp session dir, auto-generated project
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY AUTOINCREMENT, -- internal surrogate, not a business identity
    project_id TEXT    NOT NULL,                   -- stable identity, UUID v7
    name       TEXT    NOT NULL,                   -- user-visible project name
    kind       TEXT    NOT NULL,                   -- service-layer enum: standard / temp
    created_at INTEGER NOT NULL,                   -- epoch ms
    updated_at INTEGER NOT NULL                    -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_project_id_unique ON projects(project_id);
CREATE INDEX IF NOT EXISTS idx_projects_updated_at ON projects(updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_projects_kind_updated ON projects(kind, updated_at DESC);

-- Folder is the structured identity of a filesystem resource root.
-- Only the file: provider is registered for now; remote/virtual later.
-- resource_uri       = raw input provider URI; display / audit / round-trip;
--                      NOT an identity, NOT a derivation source.
-- resource_canonical = lexical canonical identity (see design "resource_canonical semantics");
--                      the unique dedupe key.
-- scheme / authority / path are NOT persisted; parsed in memory when needed.
CREATE TABLE IF NOT EXISTS folders (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT, -- internal surrogate, not a business identity
    folder_id          TEXT    NOT NULL,                  -- stable identity, UUID v7
    resource_uri       TEXT    NOT NULL,                  -- raw input provider URI
    resource_canonical TEXT    NOT NULL,                  -- lexical canonical resource URI
    created_at         INTEGER NOT NULL,                  -- epoch ms
    updated_at         INTEGER NOT NULL                   -- epoch ms
);

-- No user_id: folders are reused globally by resource_canonical.
CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_folder_id_unique ON folders(folder_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_resource_canonical_unique ON folders(resource_canonical);

-- project_explorer is the reference relation from a Project Explorer to a Folder.
-- role service-layer enum: workspace / attached
--   workspace: the single working root for all Conversations / Teams under the project
--   attached:  additional root for viewing, editing, referencing, searching
CREATE TABLE IF NOT EXISTS project_explorer (
    id           INTEGER PRIMARY KEY AUTOINCREMENT, -- internal surrogate, not a business identity
    pe_id        TEXT    NOT NULL,                   -- stable entry identity, UUID v7
    project_id   TEXT    NOT NULL,                   -- owning project id
    folder_id    TEXT    NOT NULL,                   -- referenced folder id
    role         TEXT    NOT NULL,                   -- service-layer enum: workspace / attached
    display_name TEXT,                               -- per-project display-name override; NULL derives from resource_uri / provider label
    order_index  INTEGER NOT NULL DEFAULT 0,         -- display order; workspace usually 0
    created_at   INTEGER NOT NULL,                   -- epoch ms
    updated_at   INTEGER NOT NULL                    -- epoch ms
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_pe_id_unique ON project_explorer(pe_id);
-- A project cannot reference the same folder twice.
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_project_folder_unique ON project_explorer(project_id, folder_id);
-- A project can have at most one workspace root.
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_one_workspace ON project_explorer(project_id) WHERE role = 'workspace';
-- A folder can be the workspace root of at most one project.
CREATE UNIQUE INDEX IF NOT EXISTS idx_project_explorer_one_workspace_folder ON project_explorer(folder_id) WHERE role = 'workspace';

CREATE INDEX IF NOT EXISTS idx_project_explorer_project_order ON project_explorer(project_id, order_index);
CREATE INDEX IF NOT EXISTS idx_project_explorer_folder_id ON project_explorer(folder_id);

-- Owner tables gain project_id / folder_id. Physically nullable: NULL only
-- marks legacy-unresolved rows the service layer backfills lazily. The
-- long-term business fact is folder_id + the Folder entity; conversations.extra.workspace
-- and teams.workspace are kept for the transition only.
ALTER TABLE conversations ADD COLUMN project_id TEXT;
ALTER TABLE conversations ADD COLUMN folder_id TEXT;

CREATE INDEX IF NOT EXISTS idx_conversations_project_id ON conversations(project_id);
CREATE INDEX IF NOT EXISTS idx_conversations_folder_id ON conversations(folder_id);

ALTER TABLE teams ADD COLUMN project_id TEXT;
ALTER TABLE teams ADD COLUMN folder_id TEXT;

CREATE INDEX IF NOT EXISTS idx_teams_project_id ON teams(project_id);
CREATE INDEX IF NOT EXISTS idx_teams_folder_id ON teams(folder_id);
