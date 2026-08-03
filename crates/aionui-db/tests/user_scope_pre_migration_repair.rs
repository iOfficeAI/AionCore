//! End-to-end v29-fixture upgrade tests for the migration-030 pre-migration
//! repair (Sentry ELECTRON-31Z / ELECTRON-31X).
//!
//! The bundled `DB_MIGRATOR` runs straight to 030 and `init_database_memory()`
//! starts from an empty DB, so neither enters the repair branch. These tests
//! hand-build a file-backed v29 database (migrations 001..=029 applied, so
//! `_sqlx_migrations` max version == 29) plus dirty data, then trigger
//! `init_database_staged` so the repair runs on the migrator's connection
//! immediately before 030.

use std::borrow::Cow;
use std::path::Path;

use aionui_db::init_database_staged;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

/// Build a file-backed DB with migrations 001..=29 applied (leaving 030
/// pending, so `_sqlx_migrations` max version == 29), then run each `seed`
/// statement in order. Closes the pool before returning so the caller can open
/// it via the staged-init path.
async fn build_v29_db(path: &Path, seed: &[&str]) {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let subset = full
        .migrations
        .iter()
        .filter(|m| m.version <= 29)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(subset),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(&pool).await.unwrap();
    for sql in seed {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&pool).await.ok();
    pool.close().await;
}

/// Like [`build_v29_db`] but stops at `through_version` (< 29), leaving BOTH 029
/// and 030 pending. This is the ELECTRON-31Z one-step upgrade: a DB that never
/// stopped at v29 (AionCore 0.1.52 stops at 28) jumps straight to a build with
/// 030. Staged init must run the repair before the migrator applies 029 + 030 in
/// a single pass — the path the old `== 29` gate skipped entirely.
async fn build_db_through(path: &Path, through_version: i64, seed: &[&str]) {
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let subset = full
        .migrations
        .iter()
        .filter(|m| m.version <= through_version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(subset),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    migrator.run(&pool).await.unwrap();
    for sql in seed {
        sqlx::query(sql).execute(&pool).await.unwrap();
    }
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&pool).await.ok();
    pool.close().await;
}

async fn max_applied_version(db: &aionui_db::Database) -> i64 {
    sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Highest migration version bundled in `migrations/`. Staged init runs the
/// full migrator, so a healed DB must reach exactly this version — asserting a
/// hardcoded literal (e.g. 30) breaks whenever a later migration is added.
async fn latest_bundled_version() -> i64 {
    Migrator::new(Path::new("migrations"))
        .await
        .unwrap()
        .migrations
        .iter()
        .map(|m| m.version)
        .max()
        .expect("bundled migrations are non-empty")
}

#[tokio::test]
async fn stuck_user_with_orphans_self_heals_and_030_applies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_v29_db(
        &path,
        &[
            "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES ('c_valid','system_default_user','Valid','acp','{}','pending',1,1)",
            "INSERT INTO messages (id, conversation_id, type, content, created_at) \
             VALUES ('m_valid','c_valid','user','{}',1)",
            "INSERT INTO messages (id, conversation_id, type, content, created_at) \
             VALUES ('m_orphan','c_missing','user','{}',1)",
            "INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_status, session_config) \
             VALUES ('c_missing','builtin','a','idle','{}')",
        ],
    )
    .await;

    let db = init_database_staged(&path)
        .await
        .expect("stuck DB must self-heal and finish 030");
    assert_eq!(
        max_applied_version(&db).await,
        latest_bundled_version().await,
        "migration 030+ applied after repair"
    );

    let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id='m_valid'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(valid, 1, "valid message preserved");
    let orphan: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id='m_orphan'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(orphan, 0, "orphan message removed");
    db.close().await;
}

#[tokio::test]
async fn v28_start_point_self_heals_across_029_and_030() {
    // ELECTRON-31Z root case: a DB stopped at v28 (AionCore 0.1.52) upgrades in
    // one step, so the migrator must apply BOTH 029 and 030. The widened gate
    // runs the pre-migration repair before that pass, so 030's CHECK(ok=1)
    // guards see normalized data. The old `== 29` gate skipped v28 and 030
    // aborted here (SQLite 275). Same dirty-data shape as the v29 case, but from
    // a start point that never passes through v29.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_db_through(
        &path,
        28,
        &[
            "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES ('c_valid','system_default_user','Valid','acp','{}','pending',1,1)",
            "INSERT INTO messages (id, conversation_id, type, content, created_at) \
             VALUES ('m_valid','c_valid','user','{}',1)",
            "INSERT INTO messages (id, conversation_id, type, content, created_at) \
             VALUES ('m_orphan','c_missing','user','{}',1)",
            "INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_status, session_config) \
             VALUES ('c_missing','builtin','a','idle','{}')",
        ],
    )
    .await;

    // Precondition: this really is a pre-029 start point (029 + 030 both pending).
    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        let max: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(max, 28, "fixture must start at v28 so 029 and 030 are both pending");
        pool.close().await;
    }

    let db = init_database_staged(&path)
        .await
        .expect("v28 stuck DB must self-heal across 029 + 030");
    assert_eq!(
        max_applied_version(&db).await,
        latest_bundled_version().await,
        "029 and 030 both applied after repair"
    );

    let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id='m_valid'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(valid, 1, "valid message preserved");
    let orphan: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id='m_orphan'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(orphan, 0, "orphan message removed before 030");
    db.close().await;
}

#[tokio::test]
async fn already_applied_030_upgrades_without_version_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    // Apply the FULL migrator (through 030) to simulate an already-migrated user.
    {
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new().max_connections(1).connect(&url).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
        Migrator::new(Path::new("migrations"))
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)").execute(&pool).await.ok();
        pool.close().await;
    }

    let db = init_database_staged(&path)
        .await
        .expect("already-migrated DB must open without VersionMismatch");
    assert_eq!(max_applied_version(&db).await, latest_bundled_version().await);
    db.close().await;
}

#[tokio::test]
async fn repair_then_startup_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_v29_db(
        &path,
        &[
            "INSERT INTO mailbox (id, team_id, to_agent_id, from_agent_id, type, content, created_at) \
           VALUES ('mb_orphan','t_missing','a1','a2','message','x',1)",
        ],
    )
    .await;

    let latest = latest_bundled_version().await;
    let db1 = init_database_staged(&path).await.unwrap();
    assert_eq!(max_applied_version(&db1).await, latest);
    db1.close().await;
    // Second startup on the healed DB must also succeed (gate now sees the DB
    // already past 030 → skips the repair).
    let db2 = init_database_staged(&path).await.unwrap();
    assert_eq!(max_applied_version(&db2).await, latest);
    db2.close().await;
}

#[tokio::test]
async fn defensive_dedup_prevents_unique_family_failure() {
    // spec §10 item 4: a DB whose 030 full-table rebuild would abort with the
    // UNIQUE/NOT-NULL family (not `ok = 1`) must be repaired so 030 succeeds.
    //
    // A pristine v29 chain CANNOT hold duplicate mcp_servers names: `mcp_servers`
    // carries `UNIQUE(name)` since migration 001 (`verified`:
    // 001_initial_schema.sql:381), and 030's rebuild key `UNIQUE(user_id,name)`
    // with a single owner is no wider. So the only way a real database reaches
    // 030 with duplicate names is a legacy/damaged DB whose UNIQUE was absent.
    // We simulate exactly that state: rebuild `mcp_servers` without constraints
    // via `CREATE TABLE ... AS SELECT` (which drops PK/UNIQUE), then seed the
    // duplicates. This drives the real staged-init path (pre-migration dedup →
    // 030 rebuild) and proves the defensive dedup keeps 030 from aborting.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    build_v29_db(
        &path,
        &[
            // Drop the 001-era UNIQUE(name) by recreating the table constraint-free.
            "CREATE TABLE mcp_servers_legacy AS SELECT * FROM mcp_servers",
            "DROP TABLE mcp_servers",
            "ALTER TABLE mcp_servers_legacy RENAME TO mcp_servers",
            "INSERT INTO mcp_servers (id, name, enabled, transport_type, transport_config, last_test_status, builtin, created_at, updated_at) \
             VALUES ('mcp_a','dup',1,'stdio','{}','disconnected',0,1,10)",
            "INSERT INTO mcp_servers (id, name, enabled, transport_type, transport_config, last_test_status, builtin, created_at, updated_at) \
             VALUES ('mcp_b','dup',1,'stdio','{}','disconnected',0,1,20)",
        ],
    )
    .await;

    let db = init_database_staged(&path)
        .await
        .expect("duplicate mcp name must be deduped, not abort 030");
    assert_eq!(max_applied_version(&db).await, latest_bundled_version().await);
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers WHERE name='dup'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(kept, 1, "one row kept after dedup");
    db.close().await;
}
