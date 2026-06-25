use sqlx::SqlitePool;
use tracing::info;

use crate::error::DbError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacyHandoffColumn {
    pub(crate) table: &'static str,
    pub(crate) column: &'static str,
    pub(crate) definition: &'static str,
}

pub(crate) const LEGACY_HANDOFF_COLUMNS: &[LegacyHandoffColumn] = &[
    LegacyHandoffColumn {
        table: "cron_jobs",
        column: "skill_content",
        definition: "TEXT",
    },
    LegacyHandoffColumn {
        table: "cron_jobs",
        column: "description",
        definition: "TEXT",
    },
    LegacyHandoffColumn {
        table: "conversations",
        column: "pinned",
        definition: "INTEGER NOT NULL DEFAULT 0",
    },
    LegacyHandoffColumn {
        table: "conversations",
        column: "pinned_at",
        definition: "INTEGER",
    },
    LegacyHandoffColumn {
        table: "teams",
        column: "session_mode",
        definition: "TEXT",
    },
    LegacyHandoffColumn {
        table: "teams",
        column: "agents_version",
        definition: "TEXT NOT NULL DEFAULT '1.0.0'",
    },
];

pub(crate) async fn ensure_legacy_handoff_schema(pool: &SqlitePool) -> Result<(), DbError> {
    for column in LEGACY_HANDOFF_COLUMNS {
        ensure_legacy_handoff_column(pool, *column).await?;
    }
    Ok(())
}

async fn ensure_legacy_handoff_column(pool: &SqlitePool, column: LegacyHandoffColumn) -> Result<(), DbError> {
    let table_exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?")
        .bind(column.table)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;

    if !table_exists {
        return Ok(());
    }

    let column_exists: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info(?) WHERE name = ?")
        .bind(column.table)
        .bind(column.column)
        .fetch_one(pool)
        .await
        .map_err(DbError::Query)?;

    if column_exists {
        return Ok(());
    }

    let sql = format!(
        "ALTER TABLE {} ADD COLUMN {} {}",
        column.table, column.column, column.definition
    );
    sqlx::query(&sql).execute(pool).await.map_err(DbError::Query)?;
    info!(
        table = column.table,
        column = column.column,
        "added missing legacy handoff column"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_contains_initial_handoff_columns() {
        let actual: Vec<_> = LEGACY_HANDOFF_COLUMNS
            .iter()
            .map(|column| (column.table, column.column, column.definition))
            .collect();

        assert_eq!(
            actual,
            vec![
                ("cron_jobs", "skill_content", "TEXT"),
                ("cron_jobs", "description", "TEXT"),
                ("conversations", "pinned", "INTEGER NOT NULL DEFAULT 0"),
                ("conversations", "pinned_at", "INTEGER"),
                ("teams", "session_mode", "TEXT"),
                ("teams", "agents_version", "TEXT NOT NULL DEFAULT '1.0.0'"),
            ]
        );
    }

    #[test]
    fn migration_002_direct_legacy_reads_are_audited() {
        let repaired_by_handoff_contract = [
            ("cron_jobs", "skill_content"),
            ("cron_jobs", "description"),
            ("conversations", "pinned"),
            ("conversations", "pinned_at"),
            ("teams", "session_mode"),
            ("teams", "agents_version"),
        ];

        for (table, column) in repaired_by_handoff_contract {
            assert!(
                LEGACY_HANDOFF_COLUMNS
                    .iter()
                    .any(|entry| entry.table == table && entry.column == column),
                "migration 002 reads {table}.{column}; it must stay in the handoff repair contract"
            );
        }

        // These columns are also directly read by migration 002, but they are
        // not part of this repair contract because current evidence does not
        // show compatible drift for them. Keep them documented here so review
        // of future migration-002 edits has an explicit contract decision
        // point instead of a hidden assumption.
        let documented_non_contract_reads = [
            ("messages", "hidden", "AionUi v22 adds it before v23-v26 issue path"),
            (
                "conversations",
                "source",
                "AionUi v8 baseline before v23-v26 issue path",
            ),
            (
                "conversations",
                "channel_chat_id",
                "AionUi v14 baseline before v23-v26 issue path",
            ),
            ("mailbox", "files", "AionUi v25 adds it on the observed v23->v26 path"),
            (
                "remote_agents",
                "allow_insecure",
                "AionUi v18 baseline before v23-v26 issue path",
            ),
            (
                "cron_jobs",
                "execution_mode",
                "AionUi v22 baseline before v23-v26 issue path",
            ),
            (
                "cron_jobs",
                "agent_config",
                "AionUi v22 baseline before v23-v26 issue path",
            ),
        ];

        let migration_002 = include_str!("../migrations/002_legacy_data_normalize.sql");
        for (table, column, reason) in documented_non_contract_reads {
            assert!(
                migration_002.contains(column),
                "documented migration 002 read {table}.{column} disappeared or was renamed; revisit the audit entry: {reason}"
            );
        }
    }
}
