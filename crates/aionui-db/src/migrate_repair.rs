//! Idempotent pre-migration repair for migration 030 (`user_scope`).
//!
//! Migration 030 asserts several data-integrity invariants via fail-hard
//! `CHECK (ok = 1)` temp tables. Historical local databases can carry benign
//! inconsistencies (orphan child rows, duplicate identities) that older
//! versions tolerated; those trip the assertions and abort startup with
//! SQLite error 275, permanently blocking the app (Sentry ELECTRON-31Z /
//! ELECTRON-31X).
//!
//! We cannot rewrite the shipped 030 (sqlx checksum / VersionMismatch) and a
//! forward catch-up migration cannot run (the migrator stops at the first
//! failing migration), so we normalize the raw database to satisfy 030's
//! invariants *before* the migrator runs 030 — reusing the migrator's
//! connection and the caller's cross-process startup lock.

use tracing::{info, warn};

use crate::error::DbError;

/// The migration this repair prepares for.
pub(crate) const USER_SCOPE_MIGRATION_VERSION: i64 = 30;

/// One violated invariant: a stable check name plus the count of offending
/// rows. Never carries user row content (production logging rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardViolation {
    pub check: &'static str,
    pub count: i64,
}

/// True iff migrations 001–029 are all applied and 030 is the next pending
/// migration (`max applied version == 29`). Returns false when the
/// `_sqlx_migrations` table does not exist (fresh install) — spec §7 H1.
async fn should_run_user_scope_repair(conn: &mut sqlx::SqliteConnection) -> Result<bool, DbError> {
    let has_table: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(DbError::Query)?;
    if !has_table {
        return Ok(false);
    }
    let max_version: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    Ok(max_version == Some(USER_SCOPE_MIGRATION_VERSION - 1))
}

/// Evaluate every named 030 invariant on the raw (pre-030) schema and return
/// the violated ones with counts. Task 2/3 fills in the guard list.
async fn evaluate_user_scope_guards(
    conn: &mut sqlx::SqliteConnection,
) -> Result<Vec<GuardViolation>, DbError> {
    let mut violations = Vec::new();
    for (check, count_sql) in GUARD_COUNT_QUERIES {
        let count: i64 = sqlx::query_scalar(count_sql)
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
        if count > 0 {
            violations.push(GuardViolation { check, count });
        }
    }
    Ok(violations)
}

/// (check_name, COUNT(*) query) pairs mirroring 030's guard predicates.
/// Filled in by Task 2 (A-class) and Task 3 (C-class + system_settings).
const GUARD_COUNT_QUERIES: &[(&str, &str)] = &[];

/// Gate → preflight (log names+counts) → idempotent repair. No-op off-gate.
pub(crate) async fn repair_user_scope_preconditions(
    conn: &mut sqlx::SqliteConnection,
) -> Result<(), DbError> {
    if !should_run_user_scope_repair(conn).await? {
        return Ok(());
    }

    let violations = evaluate_user_scope_guards(conn).await?;
    if violations.is_empty() {
        info!(
            migration = USER_SCOPE_MIGRATION_VERSION,
            "user_scope pre-migration preflight: no invariant violations detected"
        );
    } else {
        for v in &violations {
            warn!(
                migration = USER_SCOPE_MIGRATION_VERSION,
                check = v.check,
                count = v.count,
                "user_scope pre-migration preflight detected an invariant violation"
            );
        }
    }

    apply_user_scope_repairs(conn).await?;
    Ok(())
}

/// Apply the idempotent repairs. Filled in by Task 2 (A-class) and Task 3
/// (C-class + system_settings).
async fn apply_user_scope_repairs(_conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Connection;

    async fn conn() -> sqlx::SqliteConnection {
        sqlx::SqliteConnection::connect("sqlite::memory:").await.unwrap()
    }

    async fn seed_sqlx_migrations(c: &mut sqlx::SqliteConnection, max_version: i64) {
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT, \
             installed_on TIMESTAMP, success BOOLEAN, checksum BLOB, execution_time BIGINT)",
        )
        .execute(&mut *c)
        .await
        .unwrap();
        for v in 1..=max_version {
            sqlx::query("INSERT INTO _sqlx_migrations (version, success) VALUES (?, 1)")
                .bind(v)
                .execute(&mut *c)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn gate_false_when_no_migrations_table() {
        let mut c = conn().await;
        assert!(!should_run_user_scope_repair(&mut c).await.unwrap());
    }

    #[tokio::test]
    async fn gate_true_at_version_29() {
        let mut c = conn().await;
        seed_sqlx_migrations(&mut c, 29).await;
        assert!(should_run_user_scope_repair(&mut c).await.unwrap());
    }

    #[tokio::test]
    async fn gate_false_at_version_28_and_30() {
        let mut c = conn().await;
        seed_sqlx_migrations(&mut c, 28).await;
        assert!(!should_run_user_scope_repair(&mut c).await.unwrap());

        let mut c2 = conn().await;
        seed_sqlx_migrations(&mut c2, 30).await;
        assert!(!should_run_user_scope_repair(&mut c2).await.unwrap());
    }
}
