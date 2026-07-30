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
const GUARD_COUNT_QUERIES: &[(&str, &str)] = &[
    // --- A-class: orphan child rows (030_user_scope.sql:75-158, 787-810) ---
    ("messages_orphaned_conversation",
     "SELECT COUNT(*) FROM messages m WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = m.conversation_id)"),
    ("conversation_artifacts_orphaned_conversation",
     "SELECT COUNT(*) FROM conversation_artifacts a WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = a.conversation_id)"),
    ("conversation_assistant_snapshots_orphaned_conversation",
     "SELECT COUNT(*) FROM conversation_assistant_snapshots s WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)"),
    ("acp_session_orphaned_conversation",
     "SELECT COUNT(*) FROM acp_session s WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)"),
    ("cron_jobs_orphaned_conversation",
     "SELECT COUNT(*) FROM cron_jobs j WHERE COALESCE(j.conversation_id,'') <> '' AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = j.conversation_id)"),
    ("mailbox_orphaned_team",
     "SELECT COUNT(*) FROM mailbox m WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = m.team_id)"),
    ("team_tasks_orphaned_team",
     "SELECT COUNT(*) FROM team_tasks tt WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = tt.team_id)"),
    ("assistant_sessions_missing_owner",
     "SELECT COUNT(*) FROM assistant_sessions s WHERE NOT EXISTS (SELECT 1 FROM assistant_users u WHERE u.id = s.user_id)"),
    ("assistant_sessions_orphaned_conversation",
     "SELECT COUNT(*) FROM assistant_sessions s WHERE s.conversation_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)"),
];

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

/// Apply the idempotent repairs. Extended by Task 3 (C-class + system_settings).
async fn apply_user_scope_repairs(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    // A-class deletes — orphan child rows whose parent no longer exists.
    // Order: delete ownerless sessions BEFORE nulling orphan-conversation
    // sessions so we don't null a session we are about to delete anyway.
    for delete_sql in [
        "DELETE FROM messages WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = messages.conversation_id)",
        "DELETE FROM conversation_artifacts WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = conversation_artifacts.conversation_id)",
        "DELETE FROM conversation_assistant_snapshots WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = conversation_assistant_snapshots.conversation_id)",
        "DELETE FROM acp_session WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = acp_session.conversation_id)",
        "DELETE FROM mailbox WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = mailbox.team_id)",
        "DELETE FROM team_tasks WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = team_tasks.team_id)",
        "DELETE FROM assistant_sessions WHERE NOT EXISTS (SELECT 1 FROM assistant_users u WHERE u.id = assistant_sessions.user_id)",
    ] {
        sqlx::query(delete_sql).execute(&mut *conn).await.map_err(DbError::Query)?;
    }

    // A-class FK-null normalization — preserve user-created top-level assets.
    // cron_jobs: guard tolerates '' and NULL; use '' (codebase unanchored sentinel).
    sqlx::query(
        "UPDATE cron_jobs SET conversation_id = '' \
         WHERE COALESCE(conversation_id,'') <> '' \
           AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = cron_jobs.conversation_id)",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::Query)?;
    // assistant_sessions: guard checks IS NOT NULL, so must be NULL (not '').
    sqlx::query(
        "UPDATE assistant_sessions SET conversation_id = NULL \
         WHERE conversation_id IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = assistant_sessions.conversation_id)",
    )
    .execute(&mut *conn)
    .await
    .map_err(DbError::Query)?;

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

    async fn create_min_v29_aggregate_tables(c: &mut sqlx::SqliteConnection) {
        for ddl in [
            "CREATE TABLE conversations (id TEXT PRIMARY KEY, user_id TEXT)",
            "CREATE TABLE teams (id TEXT PRIMARY KEY)",
            "CREATE TABLE assistant_users (id TEXT PRIMARY KEY)",
            "CREATE TABLE messages (id TEXT PRIMARY KEY, conversation_id TEXT)",
            "CREATE TABLE conversation_artifacts (id TEXT PRIMARY KEY, conversation_id TEXT)",
            "CREATE TABLE conversation_assistant_snapshots (conversation_id TEXT)",
            "CREATE TABLE acp_session (conversation_id TEXT)",
            "CREATE TABLE cron_jobs (id TEXT PRIMARY KEY, conversation_id TEXT)",
            "CREATE TABLE mailbox (id TEXT PRIMARY KEY, team_id TEXT)",
            "CREATE TABLE team_tasks (id TEXT PRIMARY KEY, team_id TEXT)",
            "CREATE TABLE assistant_sessions (id TEXT PRIMARY KEY, user_id TEXT, conversation_id TEXT)",
        ] {
            sqlx::query(ddl).execute(&mut *c).await.unwrap();
        }
        sqlx::query("INSERT INTO conversations (id, user_id) VALUES ('c1','system_default_user')").execute(&mut *c).await.unwrap();
        sqlx::query("INSERT INTO teams (id) VALUES ('t1')").execute(&mut *c).await.unwrap();
        sqlx::query("INSERT INTO assistant_users (id) VALUES ('u1')").execute(&mut *c).await.unwrap();
    }

    #[tokio::test]
    async fn a_class_deletes_orphans_and_keeps_valid() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        // valid + orphan messages
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_ok','c1'),('m_orphan','missing')")
            .execute(&mut c).await.unwrap();
        sqlx::query("INSERT INTO mailbox (id, team_id) VALUES ('mb_ok','t1'),('mb_orphan','missing')")
            .execute(&mut c).await.unwrap();
        sqlx::query("INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_noowner','missing',NULL)")
            .execute(&mut c).await.unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&mut c).await.unwrap();
        assert_eq!(msgs, 1, "orphan message deleted, valid kept");
        let mbs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mailbox").fetch_one(&mut c).await.unwrap();
        assert_eq!(mbs, 1, "orphan mailbox deleted, valid kept");
        let sess: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_sessions").fetch_one(&mut c).await.unwrap();
        assert_eq!(sess, 0, "ownerless session deleted");
    }

    #[tokio::test]
    async fn a_class_nulls_fk_for_top_level_assets() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO cron_jobs (id, conversation_id) VALUES ('j_orphan','missing')")
            .execute(&mut c).await.unwrap();
        sqlx::query("INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_orphanconv','u1','missing')")
            .execute(&mut c).await.unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let cron_conv: String = sqlx::query_scalar("SELECT conversation_id FROM cron_jobs WHERE id='j_orphan'")
            .fetch_one(&mut c).await.unwrap();
        assert_eq!(cron_conv, "", "cron job preserved, conversation_id emptied");
        let cron_kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs WHERE id='j_orphan'")
            .fetch_one(&mut c).await.unwrap();
        assert_eq!(cron_kept, 1, "cron job row preserved");
        let sess_conv: Option<String> =
            sqlx::query_scalar("SELECT conversation_id FROM assistant_sessions WHERE id='s_orphanconv'")
                .fetch_one(&mut c).await.unwrap();
        assert_eq!(sess_conv, None, "session preserved, conversation_id nulled");
    }

    #[tokio::test]
    async fn a_class_repair_is_idempotent() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_orphan','missing')")
            .execute(&mut c).await.unwrap();
        apply_user_scope_repairs(&mut c).await.unwrap();
        apply_user_scope_repairs(&mut c).await.unwrap(); // second run must be a no-op
        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&mut c).await.unwrap();
        assert_eq!(msgs, 0);
    }

    #[tokio::test]
    async fn preflight_names_the_specific_violated_check() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_orphan','missing')")
            .execute(&mut c).await.unwrap();
        let violations = evaluate_user_scope_guards(&mut c).await.unwrap();
        assert!(
            violations.iter().any(|v| v.check == "messages_orphaned_conversation" && v.count == 1),
            "preflight must pinpoint the specific check by name + count, got {violations:?}"
        );
    }
}
