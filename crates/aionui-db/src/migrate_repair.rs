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
    let has_table: bool =
        sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'")
            .fetch_one(&mut *conn)
            .await
            .map_err(DbError::Query)?;
    if !has_table {
        return Ok(false);
    }
    let max_version: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(&mut *conn)
        .await
        .map_err(DbError::Query)?;
    Ok(max_version == Some(USER_SCOPE_MIGRATION_VERSION - 1))
}

/// Evaluate every named 030 invariant on the raw (pre-030) schema and return
/// the violated ones with counts. Task 2/3 fills in the guard list.
async fn evaluate_user_scope_guards(conn: &mut sqlx::SqliteConnection) -> Result<Vec<GuardViolation>, DbError> {
    let mut violations = Vec::new();
    for (check, count_sql) in GUARD_COUNT_QUERIES {
        match sqlx::query_scalar::<_, i64>(count_sql).fetch_one(&mut *conn).await {
            Ok(count) => {
                if count > 0 {
                    violations.push(GuardViolation { check, count });
                }
            }
            // A guard whose table/column is absent in this schema cannot be
            // violated (the object it protects does not exist yet). On a real
            // v29 database every referenced object exists (migrations 001–029),
            // so this only skips guards on isolated unit-test schemas.
            Err(e) if is_missing_object_error(&e) => continue,
            Err(e) => return Err(DbError::Query(e)),
        }
    }
    Ok(violations)
}

/// True when a SQLite error reports a missing schema object (the table or column
/// a guard/repair references does not exist in the current schema). On a genuine
/// v29 database every referenced object exists (migrations 001–029), so this
/// only lets the repair skip statements on isolated unit-test schemas that
/// create a subset of tables; production behavior is unchanged.
fn is_missing_object_error(err: &sqlx::Error) -> bool {
    match err.as_database_error() {
        Some(db) => {
            let message = db.message().to_ascii_lowercase();
            message.contains("no such table") || message.contains("no such column")
        }
        None => false,
    }
}

/// Execute one idempotent repair statement, tolerating a missing-object error
/// (the statement is not applicable to the current schema). See
/// [`is_missing_object_error`].
async fn exec_repair(conn: &mut sqlx::SqliteConnection, sql: &str) -> Result<(), DbError> {
    match sqlx::query(sql).execute(&mut *conn).await {
        Ok(_) => Ok(()),
        Err(e) if is_missing_object_error(&e) => Ok(()),
        Err(e) => Err(DbError::Query(e)),
    }
}

/// (check_name, COUNT(*) query) pairs mirroring 030's guard predicates.
/// Filled in by Task 2 (A-class) and Task 3 (C-class + system_settings).
const GUARD_COUNT_QUERIES: &[(&str, &str)] = &[
    // --- A-class: orphan child rows (030_user_scope.sql:75-158, 787-810) ---
    (
        "messages_orphaned_conversation",
        "SELECT COUNT(*) FROM messages m WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = m.conversation_id)",
    ),
    (
        "conversation_artifacts_orphaned_conversation",
        "SELECT COUNT(*) FROM conversation_artifacts a WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = a.conversation_id)",
    ),
    (
        "conversation_assistant_snapshots_orphaned_conversation",
        "SELECT COUNT(*) FROM conversation_assistant_snapshots s WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)",
    ),
    (
        "acp_session_orphaned_conversation",
        "SELECT COUNT(*) FROM acp_session s WHERE NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)",
    ),
    (
        "cron_jobs_orphaned_conversation",
        "SELECT COUNT(*) FROM cron_jobs j WHERE COALESCE(j.conversation_id,'') <> '' AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = j.conversation_id)",
    ),
    (
        "mailbox_orphaned_team",
        "SELECT COUNT(*) FROM mailbox m WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = m.team_id)",
    ),
    (
        "team_tasks_orphaned_team",
        "SELECT COUNT(*) FROM team_tasks tt WHERE NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = tt.team_id)",
    ),
    (
        "assistant_sessions_missing_owner",
        "SELECT COUNT(*) FROM assistant_sessions s WHERE NOT EXISTS (SELECT 1 FROM assistant_users u WHERE u.id = s.user_id)",
    ),
    (
        "assistant_sessions_orphaned_conversation",
        "SELECT COUNT(*) FROM assistant_sessions s WHERE s.conversation_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = s.conversation_id)",
    ),
    // --- C-class: duplicate composite-UNIQUE identities (030:225-263, 741-783) ---
    (
        "mcp_servers_duplicate_scope_name",
        "SELECT COUNT(*) FROM mcp_servers WHERE rowid NOT IN \
        (SELECT MIN(rowid) FROM mcp_servers GROUP BY name)",
    ),
    (
        "assistant_users_duplicate_identity",
        "SELECT COUNT(*) FROM assistant_users WHERE rowid NOT IN \
        (SELECT MIN(rowid) FROM assistant_users GROUP BY platform_user_id, platform_type)",
    ),
    // --- L4: system_settings row not keyed at id=1 would be dropped by 030 (030:318) ---
    (
        "system_settings_non_primary_id",
        "SELECT COUNT(*) FROM system_settings WHERE (SELECT COUNT(*) FROM system_settings) = 1 AND id <> 1",
    ),
    // --- B-class (DIAGNOSTIC-ONLY, no active repair): assistant_sessions cross-scope owner (030:812-824). ---
    // 030 forces assistant_users.owner_user_id='system_default_user' (030:768) but never normalizes
    // conversations.user_id, so 030's `c.user_id != u.owner_user_id` guard fails for a session-linked
    // conversation whose user_id <> 'system_default_user'. Data-dependent (spec §7.1 "不得假设一定为空"):
    // must be NAMED if it fires (spec §10 item 6 "含前置修复未覆盖的形态", §6 目标 5), even though this fix
    // performs NO active repair for it (spec §7.1 B "覆盖到即可" is soft; §7.1 L3 diagnose-if-uncovered).
    (
        "assistant_sessions_cross_scope_owner",
        "SELECT COUNT(*) FROM assistant_sessions s \
        JOIN assistant_users u ON u.id = s.user_id \
        JOIN conversations c ON c.id = s.conversation_id \
        WHERE s.conversation_id IS NOT NULL AND c.user_id <> 'system_default_user'",
    ),
];

/// Gate → preflight (log names+counts) → idempotent repair. No-op off-gate.
pub(crate) async fn repair_user_scope_preconditions(conn: &mut sqlx::SqliteConnection) -> Result<(), DbError> {
    if !should_run_user_scope_repair(conn).await? {
        return Ok(());
    }

    // Pre-repair preflight: record which named invariants are violated (check
    // name + offending-row count only, never row content) so the field-observed
    // dirty-data distribution is captured before we touch anything.
    let violations = evaluate_user_scope_guards(conn).await?;
    if violations.is_empty() {
        info!(
            migration = USER_SCOPE_MIGRATION_VERSION,
            "user_scope pre-migration preflight: no invariant violations detected"
        );
        // Nothing to repair; the repair statements are strict no-ops here. Skip
        // the post-repair re-check to avoid emitting a redundant log line.
        apply_user_scope_repairs(conn).await?;
        return Ok(());
    }
    for v in &violations {
        warn!(
            migration = USER_SCOPE_MIGRATION_VERSION,
            check = v.check,
            count = v.count,
            "user_scope pre-migration preflight detected an invariant violation"
        );
    }

    apply_user_scope_repairs(conn).await?;

    // Post-repair re-check: re-evaluate the same invariants so a subsequent 030
    // failure can be attributed to a specific STILL-violated guard (the
    // diagnostic-only B-class cross-scope owner, or a form the repair did not
    // fully cover) instead of 030's generic `CHECK constraint failed: ok = 1`.
    // This closes the "repaired vs. never-covered" ambiguity the pre-check alone
    // cannot resolve: a residual named here is exactly what will trip 030 next.
    // Reuses the same COUNT queries; runs once, on the gated upgrade only. Check
    // names + counts only (production logging rule; no user row content).
    let residual = evaluate_user_scope_guards(conn).await?;
    if residual.is_empty() {
        info!(
            migration = USER_SCOPE_MIGRATION_VERSION,
            "user_scope pre-migration repair: all detected invariant violations cleared"
        );
    } else {
        for v in &residual {
            warn!(
                migration = USER_SCOPE_MIGRATION_VERSION,
                check = v.check,
                count = v.count,
                "user_scope invariant still violated after repair; migration 030 may fail"
            );
        }
    }
    Ok(())
}

/// Apply the idempotent repairs (A-class orphan cleanup / FK-null + C-class
/// dedup + system_settings id normalization). Every statement goes through
/// [`exec_repair`], so a statement referencing a table/column absent from the
/// current schema is skipped as not-applicable (see [`is_missing_object_error`]).
/// On a real v29 database every referenced object exists, so nothing is skipped.
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
        exec_repair(conn, delete_sql).await?;
    }

    // A-class FK-null normalization — preserve user-created top-level assets.
    // cron_jobs: guard tolerates '' and NULL; use '' (codebase unanchored sentinel).
    exec_repair(
        conn,
        "UPDATE cron_jobs SET conversation_id = '' \
         WHERE COALESCE(conversation_id,'') <> '' \
           AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = cron_jobs.conversation_id)",
    )
    .await?;
    // assistant_sessions: guard checks IS NOT NULL, so must be NULL (not '').
    exec_repair(
        conn,
        "UPDATE assistant_sessions SET conversation_id = NULL \
         WHERE conversation_id IS NOT NULL \
           AND NOT EXISTS (SELECT 1 FROM conversations c WHERE c.id = assistant_sessions.conversation_id)",
    )
    .await?;

    // C-class dedup — keep the most-recently-updated row per composite-UNIQUE
    // key so 030's full-table rebuild INSERT does not hit UNIQUE(user_id,name)
    // / UNIQUE(owner_user_id,platform_user_id,platform_type). (030:241, 763.)
    // mcp_servers: all legacy rows get user_id=system_default_user in 030, so
    // the effective collision key here is `name`.
    exec_repair(
        conn,
        "DELETE FROM mcp_servers WHERE rowid NOT IN ( \
             SELECT rowid FROM ( \
                 SELECT rowid, ROW_NUMBER() OVER ( \
                     PARTITION BY name ORDER BY COALESCE(updated_at, 0) DESC, rowid DESC \
                 ) AS rn FROM mcp_servers \
             ) WHERE rn = 1 \
         )",
    )
    .await?;

    // assistant_users: collision key is (platform_user_id, platform_type).
    // Re-point dependent sessions from removed duplicates to the kept row so
    // no valid session is orphaned, THEN delete the duplicates.
    exec_repair(
        conn,
        "UPDATE assistant_sessions SET user_id = ( \
             SELECT keep.id FROM assistant_users keep \
             JOIN assistant_users dup ON dup.id = assistant_sessions.user_id \
                AND keep.platform_user_id IS dup.platform_user_id \
                AND keep.platform_type IS dup.platform_type \
             WHERE keep.rowid = ( \
                 SELECT k2.rowid FROM assistant_users k2 \
                 WHERE k2.platform_user_id IS dup.platform_user_id \
                   AND k2.platform_type IS dup.platform_type \
                 ORDER BY COALESCE(k2.authorized_at,0) DESC, k2.rowid DESC LIMIT 1 \
             ) \
         ) \
         WHERE EXISTS ( \
             SELECT 1 FROM assistant_users dup WHERE dup.id = assistant_sessions.user_id \
               AND dup.rowid <> ( \
                 SELECT k3.rowid FROM assistant_users k3 \
                 WHERE k3.platform_user_id IS dup.platform_user_id \
                   AND k3.platform_type IS dup.platform_type \
                 ORDER BY COALESCE(k3.authorized_at,0) DESC, k3.rowid DESC LIMIT 1 \
               ) \
         )",
    )
    .await?;
    exec_repair(
        conn,
        "DELETE FROM assistant_users WHERE rowid NOT IN ( \
             SELECT rowid FROM ( \
                 SELECT rowid, ROW_NUMBER() OVER ( \
                     PARTITION BY platform_user_id, platform_type \
                     ORDER BY COALESCE(authorized_at,0) DESC, rowid DESC \
                 ) AS rn FROM assistant_users \
             ) WHERE rn = 1 \
         )",
    )
    .await?;

    // L4: if exactly one system_settings row exists and it is not id=1, 030's
    // `INSERT ... WHERE id = 1` would silently drop it. Promote it to id=1.
    exec_repair(
        conn,
        "UPDATE system_settings SET id = 1 \
         WHERE (SELECT COUNT(*) FROM system_settings) = 1 AND id <> 1",
    )
    .await?;

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
        sqlx::query("INSERT INTO conversations (id, user_id) VALUES ('c1','system_default_user')")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO teams (id) VALUES ('t1')")
            .execute(&mut *c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO assistant_users (id) VALUES ('u1')")
            .execute(&mut *c)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_class_deletes_orphans_and_keeps_valid() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        // valid + orphan messages
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_ok','c1'),('m_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO mailbox (id, team_id) VALUES ('mb_ok','t1'),('mb_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_noowner','missing',NULL)",
        )
        .execute(&mut c)
        .await
        .unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(msgs, 1, "orphan message deleted, valid kept");
        let mbs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mailbox")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(mbs, 1, "orphan mailbox deleted, valid kept");
        let sess: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assistant_sessions")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(sess, 0, "ownerless session deleted");
    }

    #[tokio::test]
    async fn a_class_nulls_fk_for_top_level_assets() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO cron_jobs (id, conversation_id) VALUES ('j_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_orphanconv','u1','missing')",
        )
        .execute(&mut c)
        .await
        .unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let cron_conv: String = sqlx::query_scalar("SELECT conversation_id FROM cron_jobs WHERE id='j_orphan'")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(cron_conv, "", "cron job preserved, conversation_id emptied");
        let cron_kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cron_jobs WHERE id='j_orphan'")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(cron_kept, 1, "cron job row preserved");
        let sess_conv: Option<String> =
            sqlx::query_scalar("SELECT conversation_id FROM assistant_sessions WHERE id='s_orphanconv'")
                .fetch_one(&mut c)
                .await
                .unwrap();
        assert_eq!(sess_conv, None, "session preserved, conversation_id nulled");
    }

    #[tokio::test]
    async fn a_class_repair_is_idempotent() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        apply_user_scope_repairs(&mut c).await.unwrap();
        apply_user_scope_repairs(&mut c).await.unwrap(); // second run must be a no-op
        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(msgs, 0);
    }

    #[tokio::test]
    async fn preflight_names_the_specific_violated_check() {
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        let violations = evaluate_user_scope_guards(&mut c).await.unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v.check == "messages_orphaned_conversation" && v.count == 1),
            "preflight must pinpoint the specific check by name + count, got {violations:?}"
        );
    }

    #[tokio::test]
    async fn c_class_dedupes_mcp_servers_by_scope_name() {
        let mut c = conn().await;
        sqlx::query("CREATE TABLE mcp_servers (id TEXT PRIMARY KEY, name TEXT, updated_at INTEGER)")
            .execute(&mut c)
            .await
            .unwrap();
        // Two rows share name; keep the most-recently-updated.
        sqlx::query("INSERT INTO mcp_servers (id, name, updated_at) VALUES ('old','dup',10),('new','dup',20),('solo','unique',5)")
            .execute(&mut c).await.unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let kept: Vec<String> = sqlx::query_scalar("SELECT id FROM mcp_servers ORDER BY id")
            .fetch_all(&mut c)
            .await
            .unwrap();
        assert_eq!(
            kept,
            vec!["new".to_string(), "solo".to_string()],
            "duplicate name deduped, newest kept"
        );
    }

    #[tokio::test]
    async fn c_class_dedupes_assistant_users_and_repoints_sessions() {
        let mut c = conn().await;
        sqlx::query("CREATE TABLE assistant_users (id TEXT PRIMARY KEY, platform_user_id TEXT, platform_type TEXT, authorized_at INTEGER)")
            .execute(&mut c).await.unwrap();
        sqlx::query("CREATE TABLE assistant_sessions (id TEXT PRIMARY KEY, user_id TEXT, conversation_id TEXT)")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO assistant_users (id, platform_user_id, platform_type, authorized_at) VALUES ('au_old','p','telegram',10),('au_new','p','telegram',20)")
            .execute(&mut c).await.unwrap();
        sqlx::query("INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('sess','au_old',NULL)")
            .execute(&mut c)
            .await
            .unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let users: Vec<String> = sqlx::query_scalar("SELECT id FROM assistant_users")
            .fetch_all(&mut c)
            .await
            .unwrap();
        assert_eq!(
            users,
            vec!["au_new".to_string()],
            "duplicate identity deduped, newest kept"
        );
        let sess_owner: String = sqlx::query_scalar("SELECT user_id FROM assistant_sessions WHERE id='sess'")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(sess_owner, "au_new", "session re-pointed to kept owner (not orphaned)");
    }

    #[tokio::test]
    async fn c_class_promotes_lone_system_settings_row_to_id_1() {
        let mut c = conn().await;
        sqlx::query("CREATE TABLE system_settings (id INTEGER PRIMARY KEY, language TEXT, updated_at INTEGER)")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO system_settings (id, language, updated_at) VALUES (5,'zh-CN',1)")
            .execute(&mut c)
            .await
            .unwrap();

        apply_user_scope_repairs(&mut c).await.unwrap();

        let id: i64 = sqlx::query_scalar("SELECT id FROM system_settings")
            .fetch_one(&mut c)
            .await
            .unwrap();
        assert_eq!(
            id, 1,
            "lone settings row promoted to id=1 so 030 WHERE id=1 copy preserves it"
        );
    }

    #[tokio::test]
    async fn post_repair_recheck_reports_residual_after_clearing_repaired_guards() {
        // The post-repair re-check re-evaluates the same guards after repair so a
        // residual (e.g. diagnostic-only B-class) stays attributable while a
        // repaired A-class guard reads clean. This asserts that exact sequence —
        // the logic the "still violated after repair" warn / "all cleared" info
        // lines report — without needing a log-capture harness.
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        // A-class orphan (repaired) + B-class cross-scope session (diagnostic-only).
        sqlx::query("INSERT INTO messages (id, conversation_id) VALUES ('m_orphan','missing')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id, user_id) VALUES ('c_other','user-abc')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_x','u1','c_other')")
            .execute(&mut c)
            .await
            .unwrap();

        let pre = evaluate_user_scope_guards(&mut c).await.unwrap();
        assert!(
            pre.iter().any(|v| v.check == "messages_orphaned_conversation"),
            "A-class orphan must be detected pre-repair, got {pre:?}"
        );
        assert!(
            pre.iter().any(|v| v.check == "assistant_sessions_cross_scope_owner"),
            "B-class must be detected pre-repair, got {pre:?}"
        );

        apply_user_scope_repairs(&mut c).await.unwrap();

        let residual = evaluate_user_scope_guards(&mut c).await.unwrap();
        assert!(
            !residual.iter().any(|v| v.check == "messages_orphaned_conversation"),
            "repaired A-class orphan must NOT appear in the post-repair residual, got {residual:?}"
        );
        assert!(
            residual
                .iter()
                .any(|v| v.check == "assistant_sessions_cross_scope_owner" && v.count == 1),
            "diagnostic-only B-class must remain a named residual after repair, got {residual:?}"
        );
    }

    #[tokio::test]
    async fn preflight_names_uncovered_cross_scope_guard() {
        // spec §10 item 6: a DELIBERATELY-UNCOVERED (B-class) guard failure must still be NAMED.
        let mut c = conn().await;
        create_min_v29_aggregate_tables(&mut c).await;
        // A conversation owned by a non-default user, linked by a session with a valid owner.
        sqlx::query("INSERT INTO conversations (id, user_id) VALUES ('c_other','user-abc')")
            .execute(&mut c)
            .await
            .unwrap();
        sqlx::query("INSERT INTO assistant_sessions (id, user_id, conversation_id) VALUES ('s_x','u1','c_other')")
            .execute(&mut c)
            .await
            .unwrap();

        let violations = evaluate_user_scope_guards(&mut c).await.unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v.check == "assistant_sessions_cross_scope_owner" && v.count == 1),
            "uncovered cross-scope guard must be named in the preflight, got {violations:?}"
        );

        // B-class is diagnostic-only: apply_user_scope_repairs must NOT mutate it away.
        apply_user_scope_repairs(&mut c).await.unwrap();
        let still: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assistant_sessions s JOIN conversations c ON c.id = s.conversation_id \
             WHERE c.user_id <> 'system_default_user'",
        )
        .fetch_one(&mut c)
        .await
        .unwrap();
        assert_eq!(
            still, 1,
            "B-class cross-scope divergence is named but not repaired in this fix"
        );
    }
}
