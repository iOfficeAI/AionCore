-- Persist the OAuth client credentials obtained via Dynamic Client Registration
-- (RFC 7591) for an MCP server.
--
-- Before this, MCP OAuth always used a hardcoded public client id, so any
-- authorization server that requires registration rejected the flow with
-- `invalid_client`. The client id (and the secret, when the server issues one)
-- is per authorization-server and must survive restarts: the refresh-token
-- grant has to present the SAME client that obtained the token.
--
-- Columns are nullable: a server without a `registration_endpoint` keeps using
-- the static fallback client id, and rows written before this migration simply
-- have no registered client.
--
-- SQLite `ADD COLUMN` does not support `IF NOT EXISTS`; each migration runs
-- exactly once (tracked in `_sqlx_migrations`), matching the 042 convention.
ALTER TABLE oauth_tokens ADD COLUMN client_id TEXT;
ALTER TABLE oauth_tokens ADD COLUMN client_secret TEXT;
