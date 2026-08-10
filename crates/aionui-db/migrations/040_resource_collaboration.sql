-- Phase 2 multi-user resource collaboration (explicit shares).

CREATE TABLE resource_shares (
  id TEXT PRIMARY KEY,
  resource_type TEXT NOT NULL CHECK (resource_type IN ('conversation','project','provider')),
  resource_id TEXT NOT NULL,
  owner_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  grantee_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  permission TEXT NOT NULL CHECK (permission IN ('view','edit')),
  created_at INTEGER NOT NULL,
  created_by TEXT NOT NULL REFERENCES users(id),
  UNIQUE(resource_type, resource_id, grantee_user_id),
  CHECK (owner_user_id != grantee_user_id)
);

CREATE INDEX idx_resource_shares_grantee ON resource_shares(grantee_user_id, resource_type);
CREATE INDEX idx_resource_shares_resource ON resource_shares(resource_type, resource_id);
