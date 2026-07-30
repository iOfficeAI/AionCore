-- Migration 032: one-shot marker for the AionUi -> AionPro data adoption.
--
-- The adoption window used to be a pure eligibility predicate ("exactly one
-- external user, and it is the caller"), evaluated on EVERY provision call.
-- That guards WHO may adopt (a second account can never inherit another
-- user's data) but not HOW MANY TIMES: as long as the machine kept a single
-- external account, every login re-swept whatever the local default user had
-- accumulated since — new AionUi conversations were repeatedly re-owned.
--
-- These columns live on the `system_default_user` row and record that the
-- one-time adoption has happened (by whom, when). The window predicate gains
-- a third condition (`adopted_by IS NULL`), so adoption fires exactly once —
-- on the first external account's first login — and later local-mode data
-- stays local. Rows for other users keep NULL in both columns.
--
-- Existing databases are not backfilled: their next provision performs the
-- final sweep under the old semantics and stamps the marker in the same
-- transaction.
ALTER TABLE users ADD COLUMN adopted_by TEXT;
ALTER TABLE users ADD COLUMN adopted_at INTEGER;
