use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fs2::FileExt;
use sqlx::migrate::Migrator;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Sqlite, SqlitePool};
use tracing::{info, warn};

use crate::error::DbError;

/// Maximum number of connections in the pool.
const MAX_CONNECTIONS: u32 = 5;

/// SQLite busy timeout in milliseconds.
const BUSY_TIMEOUT_MS: u64 = 5000;
const STARTUP_FILE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];

static DB_MIGRATOR: Migrator = sqlx::migrate!();
// Historical special-case for the MCP schema reconciliation fallback.
// Keep this pinned to migration version 7 even as newer migrations land.
const MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION: i64 = 7;
const RECOVERABLE_DATABASE_CORRUPTION_STAGE: &str = "database.recoverable_corruption";

/// Wraps a SQLite connection pool with lifecycle management.
#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DatabaseInitOptions {
    pub recover_corrupted_database: bool,
}

#[derive(Debug)]
pub struct DatabaseInitError {
    stage: &'static str,
    source: DbError,
}

impl DatabaseInitError {
    pub fn new(stage: &'static str, source: DbError) -> Self {
        Self { stage, source }
    }

    pub fn stage(&self) -> &'static str {
        self.stage
    }

    pub fn into_source(self) -> DbError {
        self.source
    }

    fn source(&self) -> &DbError {
        &self.source
    }
}

impl std::fmt::Display for DatabaseInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.stage, self.source)
    }
}

impl std::error::Error for DatabaseInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl Database {
    /// Returns a reference to the underlying connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes all connections in the pool.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Initialize a file-backed SQLite database.
///
/// Creates the database file and parent directories if they don't exist,
/// configures pragmas (foreign_keys, busy_timeout, journal_mode=WAL),
/// runs migrations, and ensures the system default user exists.
///
/// If initialization fails on an existing file, only explicit corruption-like
/// failures attempt recovery by backing up the corrupted file and creating a
/// fresh database. Migration mismatches and lock contention fail fast.
pub async fn init_database(path: &Path) -> Result<Database, DbError> {
    init_database_with_options(path, DatabaseInitOptions::default())
        .await
        .map_err(DatabaseInitError::into_source)
}

pub async fn init_database_with_options(
    path: &Path,
    options: DatabaseInitOptions,
) -> Result<Database, DatabaseInitError> {
    init_database_staged_with_options(path, options).await
}

pub async fn init_database_staged(path: &Path) -> Result<Database, DatabaseInitError> {
    init_database_staged_with_options(path, DatabaseInitOptions::default()).await
}

pub async fn init_database_staged_with_options(
    path: &Path,
    options: DatabaseInitOptions,
) -> Result<Database, DatabaseInitError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            DatabaseInitError::new(
                "database.open",
                DbError::Init(format!("Failed to create database directory: {e}")),
            )
        })?;
    }

    match try_init_file_staged(path).await {
        Ok(db) => Ok(db),
        Err(e) if path.exists() && options.recover_corrupted_database && should_attempt_recovery(e.source()) => {
            warn!(
                code = "BOOTSTRAP_DATABASE_CORRUPTION_REBUILD_AUTHORIZED",
                stage = e.stage(),
                "Authorized corrupted database backup and rebuild"
            );
            recover_and_retry(path, e.into_source()).await
        }
        Err(e) if path.exists() && is_recoverable_migration_corruption(e.source()) => {
            warn!(
                code = "BOOTSTRAP_DATABASE_CORRUPTION_REQUIRES_USER_CONFIRMATION",
                stage = e.stage(),
                "Database corruption-like migration failure requires user confirmation before rebuild"
            );
            Err(DatabaseInitError::new(
                RECOVERABLE_DATABASE_CORRUPTION_STAGE,
                e.into_source(),
            ))
        }
        Err(e) => Err(e),
    }
}

/// Initialize an in-memory SQLite database (for testing).
///
/// Uses a single connection to ensure all queries share the same in-memory database.
/// Note: WAL journal mode is not available for in-memory databases.
pub async fn init_database_memory() -> Result<Database, DbError> {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:")
        .map_err(|e| DbError::Init(format!("Invalid memory connection string: {e}")))?
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS));

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .map_err(DbError::Query)?;

    // In-memory DBs are not shared across processes, so no advisory lock is
    // needed (and there is no on-disk path we could create one against).
    run_migrations(&pool).await?;
    ensure_system_user(&pool).await?;

    info!("In-memory database initialized");
    Ok(Database { pool })
}

/// Copy the legacy `aionui.db` to the new target path if the target does not exist.
///
/// This enables safe upgrades: the old database remains untouched and the backend
/// operates exclusively on the copy. The copy is atomic (write to `.tmp`, then rename)
/// so a crash mid-copy leaves no half-written target file.
pub fn maybe_copy_legacy_database(target: &Path) -> Result<(), DbError> {
    if target.exists() {
        return Ok(());
    }

    let legacy = target.with_file_name("aionui.db");
    if !legacy.exists() {
        return Ok(());
    }

    let lock_path = migrate_lock_path(target);
    let _guard = match MigrateLockGuard::acquire(&lock_path) {
        Ok(guard) => Some(guard),
        Err(e) => {
            warn!(
                lock = %lock_path.display(),
                error = %e,
                "Could not acquire legacy database copy lock; continuing without it"
            );
            None
        }
    };
    if target.exists() {
        return Ok(());
    }

    let tmp = target.with_extension("db.tmp");
    retry_startup_file_op("copy legacy database", &legacy, || std::fs::copy(&legacy, &tmp))
        .map_err(|e| DbError::Init(format!("Failed to copy legacy database: {e}")))?;
    if target.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(());
    }
    match retry_startup_file_op("rename temp database", &tmp, || std::fs::rename(&tmp, target)) {
        Ok(()) => {}
        Err(e) if target.exists() => {
            warn!(
                target = %target.display(),
                tmp = %tmp.display(),
                error = %e,
                "Legacy database target appeared after rename failed; using existing target"
            );
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => return Err(DbError::Init(format!("Failed to rename temp database: {e}"))),
    }

    let _ = std::fs::remove_file(target.with_extension("db-wal"));
    let _ = std::fs::remove_file(target.with_extension("db-shm"));

    info!("Copied legacy database {} -> {}", legacy.display(), target.display());
    Ok(())
}

async fn try_init_file_staged(path: &Path) -> Result<Database, DatabaseInitError> {
    // Serialize the whole file-backed startup path, not only the sqlx
    // migrator. Opening a fresh SQLite file also runs connection-level PRAGMAs
    // such as WAL setup, which can race before migrations start.
    let lock_path = migrate_lock_path(path);
    let _guard = match MigrateLockGuard::acquire(&lock_path) {
        Ok(guard) => Some(guard),
        Err(e) => {
            // Don't fail startup if flock isn't available (e.g. on some
            // network filesystems) - fall back to SQLite busy-timeout and
            // retry-on-conflict behavior below.
            warn!("Could not acquire database startup lock {}: {e}", lock_path.display());
            None
        }
    };

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .journal_mode(SqliteJournalMode::Wal);

    let pool = PoolOptions::<Sqlite>::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(opts)
        .await
        .map_err(|e| DatabaseInitError::new("database.open", DbError::Query(e)))?;

    run_migrations_staged(&pool).await?;
    ensure_system_user(&pool)
        .await
        .map_err(|e| DatabaseInitError::new("database.seed", e))?;

    info!("Database initialized at {}", path.display());
    Ok(Database { pool })
}

/// Path of the cross-process advisory lock file used to serialize concurrent
/// migrators on the same database.
///
/// We put it next to the DB file so it lives on the same filesystem (avoids
/// odd flock semantics across mount points) and gets cleaned up alongside the
/// DB if a user resets their data directory.
fn migrate_lock_path(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    let new_name = match p.file_name().and_then(|s| s.to_str()) {
        Some(name) => format!("{name}.migrate.lock"),
        None => "aionui.migrate.lock".to_string(),
    };
    p.set_file_name(new_name);
    p
}

fn retry_startup_file_op<T, F>(operation: &str, path: &Path, mut op: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    for (attempt, delay) in STARTUP_FILE_RETRY_DELAYS.iter().enumerate() {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if is_retryable_startup_file_error(&e) => {
                warn!(
                    operation,
                    path = %path.display(),
                    attempt = attempt + 1,
                    retry_after_ms = delay.as_millis(),
                    raw_os_error = ?e.raw_os_error(),
                    error = %e,
                    "Startup file operation failed; retrying"
                );
                std::thread::sleep(*delay);
            }
            Err(e) => return Err(e),
        }
    }
    op()
}

fn is_retryable_startup_file_error(error: &std::io::Error) -> bool {
    match error.kind() {
        std::io::ErrorKind::Interrupted
        | std::io::ErrorKind::PermissionDenied
        | std::io::ErrorKind::TimedOut
        | std::io::ErrorKind::WouldBlock => true,
        _ => matches!(error.raw_os_error(), Some(5 | 32 | 33)),
    }
}

async fn run_migrations(pool: &SqlitePool) -> Result<(), DbError> {
    run_migrations_staged(pool)
        .await
        .map_err(DatabaseInitError::into_source)
}

async fn run_migrations_staged(pool: &SqlitePool) -> Result<(), DatabaseInitError> {
    // File-backed callers hold a cross-process startup lock before opening the
    // SQLite pool. sqlx-sqlite's Migrate impl has no-op
    // lock()/unlock() and the migrator does list_applied → apply without an
    // outer transaction, so two processes opening the same DB simultaneously
    // (e.g. Electron auto-update spawning v2.1.1 while v2.0.x is still
    // shutting down, or `aioncore doctor` racing the server) can both decide
    // to apply the same version and the slower one's INSERT into
    // `_sqlx_migrations` blows up with `UNIQUE constraint failed:
    // _sqlx_migrations.version`. The outer startup lock also covers
    // schema-repair and connection PRAGMAs before migration execution.
    ensure_schema_columns(pool)
        .await
        .map_err(|e| DatabaseInitError::new("database.schema_repair", e))?;
    // Migration 002 rebuilds tables via RENAME+DROP. Two pragmas are needed:
    // - foreign_keys=OFF: prevents DROP TABLE from triggering ON DELETE CASCADE
    // - legacy_alter_table=ON: prevents ALTER TABLE RENAME from rewriting FK
    //   references in other tables (SQLite 3.26+ rewrites them by default)
    // Both must be set outside a transaction (sqlx wraps each migration in one).
    let mut conn = pool
        .acquire()
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&mut *conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;

    let result = run_migrations_with_retry(&mut conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", e));

    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&mut *conn)
        .await
        .map_err(|e| DatabaseInitError::new("database.migration", DbError::Query(e)))?;
    result
}

/// Run sqlx migrations with one retry on `_sqlx_migrations` UNIQUE conflict.
///
/// The advisory file lock above already serialises well-behaved processes,
/// but a UNIQUE conflict can still leak through when:
/// - flock() failed (network FS, sandbox restrictions) and we proceeded.
/// - Two processes that both bypassed the lock raced.
///
/// In every UNIQUE-conflict scenario the failing migration's transaction was
/// rolled back, so re-running `sqlx::migrate!().run` is safe: the second
/// pass sees the row that the winner committed, checksum matches (same
/// shipped binary), and the migration is treated as already applied.
async fn run_migrations_with_retry(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    match DB_MIGRATOR.run(&mut *conn).await {
        Ok(()) => Ok(()),
        Err(e) if is_migrations_table_unique_conflict(&e) => {
            warn!("Concurrent migrator detected (UNIQUE conflict on _sqlx_migrations); retrying");
            DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
        }
        Err(sqlx::migrate::MigrateError::VersionMismatch(version))
            if version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION =>
        {
            if align_reconciled_mcp_migration_checksum(&mut *conn).await? {
                warn!(
                    "Aligned checksum for reconciled MCP migration {}; retrying",
                    MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION
                );
                DB_MIGRATOR.run(&mut *conn).await.map_err(DbError::Migration)
            } else {
                Err(DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(
                    version,
                )))
            }
        }
        Err(e) => Err(DbError::Migration(e)),
    }
}

/// Detect the specific "another process inserted this version first" error.
///
/// sqlx wraps the SQLite error inside `MigrateError::Execute(sqlx::Error)`.
/// We match on the textual message rather than the SQLite extended error code
/// because sqlx loses the structured code by the time it bubbles up here.
fn is_migrations_table_unique_conflict(err: &sqlx::migrate::MigrateError) -> bool {
    let msg = err.to_string();
    msg.contains("UNIQUE constraint failed: _sqlx_migrations.version")
}

/// RAII guard that holds an exclusive file lock for the lifetime of the
/// migration run. Drop unlocks and best-effort closes the file handle.
struct MigrateLockGuard {
    file: std::fs::File,
}

impl MigrateLockGuard {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // Blocking lock — fs2 has no async variant. We're inside an async
        // context but startup blocks anyway and the critical section is
        // bounded (single-process migration run), so this is acceptable.
        FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for MigrateLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Ensure columns expected by Rust models exist in the database.
///
/// `CREATE TABLE IF NOT EXISTS` does not modify existing tables, so columns
/// added after a table was first created may be missing. This function
/// safely adds any missing columns via `ALTER TABLE ADD COLUMN`.
async fn ensure_schema_columns(pool: &SqlitePool) -> Result<(), DbError> {
    reconcile_mcp_server_schema(pool).await?;
    crate::legacy_handoff::ensure_legacy_handoff_schema(pool).await?;
    Ok(())
}

async fn reconcile_mcp_server_schema(pool: &SqlitePool) -> Result<(), DbError> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='mcp_servers'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    if !table_exists {
        return Ok(());
    }

    let has_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'status'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    let has_last_test_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'last_test_status'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;

    let has_deleted_at: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'deleted_at'")
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;

    let clean_pre_migration = has_status && !has_last_test_status && !has_deleted_at;
    if clean_pre_migration {
        return Ok(());
    }

    if has_status && !has_last_test_status {
        sqlx::query("ALTER TABLE mcp_servers RENAME COLUMN status TO last_test_status")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Renamed mcp_servers.status to last_test_status");
    } else if !has_status && !has_last_test_status {
        sqlx::query("ALTER TABLE mcp_servers ADD COLUMN last_test_status TEXT NOT NULL DEFAULT 'disconnected'")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Added missing column mcp_servers.last_test_status");
    }

    if !has_deleted_at {
        sqlx::query("ALTER TABLE mcp_servers ADD COLUMN deleted_at INTEGER")
            .execute(pool)
            .await
            .map_err(DbError::Query)?;
        info!("Added missing column mcp_servers.deleted_at");
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_mcp_servers_deleted_at ON mcp_servers(deleted_at)")
        .execute(pool)
        .await
        .map_err(DbError::Query)?;

    record_reconciled_mcp_migration(pool).await?;

    Ok(())
}

async fn record_reconciled_mcp_migration(pool: &SqlitePool) -> Result<(), DbError> {
    let Some(migration) = DB_MIGRATOR
        .iter()
        .find(|migration| migration.version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
    else {
        return Ok(());
    };

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version BIGINT PRIMARY KEY,
    description TEXT NOT NULL,
    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    success BOOLEAN NOT NULL,
    checksum BLOB NOT NULL,
    execution_time BIGINT NOT NULL
)
        "#,
    )
    .execute(pool)
    .await
    .map_err(DbError::Query)?;

    let already_applied: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM _sqlx_migrations WHERE version = ? AND success = 1")
            .bind(MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
            .fetch_one(pool)
            .await
            .map_err(DbError::Query)?;
    if already_applied {
        return Ok(());
    }

    sqlx::query(
        r#"
INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
VALUES (?, ?, TRUE, ?, 0)
        "#,
    )
    .bind(migration.version)
    .bind(&*migration.description)
    .bind(&*migration.checksum)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    info!("Recorded reconciled MCP schema migration {}", migration.version);
    Ok(())
}

async fn align_reconciled_mcp_migration_checksum(conn: &mut sqlx::SqliteConnection) -> Result<bool, DbError> {
    let has_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'status'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    let has_last_test_status: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'last_test_status'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    let has_deleted_at: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('mcp_servers') WHERE name = 'deleted_at'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;

    if has_status || !has_last_test_status || !has_deleted_at {
        return Ok(false);
    }

    let Some(migration) = DB_MIGRATOR
        .iter()
        .find(|migration| migration.version == MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
    else {
        return Ok(false);
    };

    let updated = sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
        .bind(&*migration.checksum)
        .bind(MCP_SCHEMA_RECONCILIATION_MIGRATION_VERSION)
        .execute(&mut *conn)
        .await
        .map_err(DbError::Query)?;

    Ok(updated.rows_affected() > 0)
}

/// Ensure the system default user exists.
///
/// Uses INSERT OR IGNORE so it is safe to call on every startup.
/// The system user has an empty password hash, which signals "needs setup".
/// Username defaults to `admin` — matches the legacy web-host login flow so
/// users upgrading from pre-M6 builds keep the same login username.
async fn ensure_system_user(pool: &SqlitePool) -> Result<(), DbError> {
    let now = aionui_common::now_ms();
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("system_default_user")
    .bind("admin")
    .bind("")
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(DbError::Query)?;
    Ok(())
}

async fn recover_and_retry(path: &Path, original_error: DbError) -> Result<Database, DatabaseInitError> {
    let backup_path = format!("{}.backup.{}", path.display(), aionui_common::now_ms());
    warn!("Backing up corrupted database to: {backup_path}");

    std::fs::rename(path, &backup_path).map_err(|e| {
        DatabaseInitError::new(
            "database.recovery",
            DbError::Init(format!(
                "Recovery failed: could not backup corrupted database: {e}. \
                 Original error: {original_error}"
            )),
        )
    })?;

    match try_init_file_staged(path).await {
        Ok(db) => {
            warn!(
                code = "BOOTSTRAP_RECOVERED_DATABASE_CORRUPTION",
                stage = "database.recovery",
                backup_path = %backup_path,
                "Database recovered after corruption-like startup failure"
            );
            Ok(db)
        }
        Err(retry_err) => Err(DatabaseInitError::new(
            "database.recovery",
            DbError::Init(format!(
                "Recovery failed after backup: {retry_err}. Original error: {original_error}"
            )),
        )),
    }
}

fn should_attempt_recovery(err: &DbError) -> bool {
    match err {
        DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(_)) => false,
        DbError::Migration(_) => is_corruption_like_error(err),
        DbError::NotFound(_) | DbError::Conflict(_) => false,
        DbError::Query(_) | DbError::Init(_) => is_corruption_like_error(err),
    }
}

fn is_recoverable_migration_corruption(err: &DbError) -> bool {
    match err {
        DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(_)) => false,
        DbError::Migration(_) => is_corruption_like_error(err),
        _ => false,
    }
}

fn is_corruption_like_error(err: &DbError) -> bool {
    let message = err.to_string().to_ascii_lowercase();

    [
        "sqlite_corrupt",
        "database disk image is malformed",
        "file is not a database",
        "sqlite_notadb",
        "malformed database schema",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_skips_migration_version_mismatch() {
        let err = DbError::Migration(sqlx::migrate::MigrateError::VersionMismatch(6));

        assert!(
            !should_attempt_recovery(&err),
            "migration checksum mismatch must not trigger recovery"
        );
    }

    #[test]
    fn recovery_skips_lock_contention_errors() {
        let err = DbError::Init("database is locked".into());

        assert!(
            !should_attempt_recovery(&err),
            "lock contention must not trigger recovery"
        );
    }

    #[test]
    fn recovery_allows_corruption_like_errors() {
        let err = DbError::Init("database disk image is malformed".into());

        assert!(
            should_attempt_recovery(&err),
            "corruption-like failures should trigger recovery"
        );
    }

    #[test]
    fn recovery_allows_corruption_like_migration_errors_when_authorized() {
        let err = DbError::Migration(sqlx::migrate::MigrateError::ExecuteMigration(
            sqlx::Error::Protocol("database disk image is malformed".into()),
            13,
        ));

        assert!(should_attempt_recovery(&err));
        assert!(is_recoverable_migration_corruption(&err));
    }

    #[test]
    fn recovery_skips_non_corruption_migration_errors() {
        let err = DbError::Migration(sqlx::migrate::MigrateError::ExecuteMigration(
            sqlx::Error::Protocol("UNIQUE constraint failed: tasks.id".into()),
            13,
        ));

        assert!(!should_attempt_recovery(&err));
        assert!(!is_recoverable_migration_corruption(&err));
    }

    #[tokio::test]
    async fn migration_preserves_fk_references() {
        let db = init_database_memory().await.unwrap();
        let pool = db.pool();

        let fk_table: String = sqlx::query_scalar(
            "SELECT \"table\" FROM pragma_foreign_key_list('messages') WHERE \"from\"='conversation_id'",
        )
        .fetch_one(pool)
        .await
        .unwrap();

        assert_eq!(fk_table, "conversations");
    }

    #[test]
    fn migrations_table_unique_conflict_detected_from_message() {
        // Build the same Execute(sqlx::Error) shape that surfaces when two
        // processes race on `INSERT INTO _sqlx_migrations`. The detector has
        // to match on the textual message because the SQLite extended code
        // is not preserved on the path through MigrateError.
        let inner = sqlx::Error::Protocol("UNIQUE constraint failed: _sqlx_migrations.version".to_string());
        let err = sqlx::migrate::MigrateError::Execute(inner);
        assert!(is_migrations_table_unique_conflict(&err));
    }

    #[test]
    fn migrations_table_unique_conflict_ignores_other_errors() {
        let other = sqlx::migrate::MigrateError::VersionMismatch(3);
        assert!(!is_migrations_table_unique_conflict(&other));

        let unrelated = sqlx::migrate::MigrateError::Execute(sqlx::Error::Protocol(
            "UNIQUE constraint failed: users.username".to_string(),
        ));
        assert!(!is_migrations_table_unique_conflict(&unrelated));
    }

    #[test]
    fn migrate_lock_path_sits_next_to_db() {
        let db = Path::new("/var/lib/aionui/aionui-backend.db");
        let lock = migrate_lock_path(db);
        assert_eq!(lock.parent(), db.parent());
        assert_eq!(lock.file_name().unwrap(), "aionui-backend.db.migrate.lock");
    }

    #[test]
    fn startup_file_retry_handles_windows_transient_lock_errors() {
        for code in [5, 32, 33] {
            let err = std::io::Error::from_raw_os_error(code);
            assert!(
                is_retryable_startup_file_error(&err),
                "Windows startup file error {code} should be retryable"
            );
        }
    }

    #[test]
    fn startup_file_retry_rejects_non_transient_errors() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        assert!(!is_retryable_startup_file_error(&err));
    }
}
