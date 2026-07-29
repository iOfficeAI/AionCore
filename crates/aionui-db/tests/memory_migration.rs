use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;

use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

use aionui_db::{ConsumeMemoryRetrievalSnapshotRow, DbError, IMemoryRepository, SqliteMemoryRepository};

async fn run_migrations_through(pool: &sqlx::SqlitePool, max_version: i64) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| migration.version <= max_version)
        .cloned()
        .collect::<Vec<_>>();
    let migrator = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    let mut connection = pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&mut *connection)
        .await
        .unwrap();
    migrator.run(&mut *connection).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&mut *connection)
        .await
        .unwrap();
}

#[tokio::test]
async fn migration_031_upgrades_030_and_preserves_legacy_messages_with_null_turn_id() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 30).await;
    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, created_at, updated_at)
         VALUES ('system_default_user', 'system', 'system@aionui.local', '', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('legacy-conv', 'system_default_user', 'Legacy', 'gemini', '{}', 'finished', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, type, content, hidden, created_at)
         VALUES ('legacy-msg', 'legacy-conv', 'text', '{}', 0, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migrations_through(&pool, 31).await;

    let turn_id: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id = 'legacy-msg'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(turn_id, None);
}

#[tokio::test]
async fn migration_032_assigns_non_reusable_sequences_to_existing_and_new_conversations() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 31).await;
    sqlx::query(
        "INSERT INTO users (id,username,email,password_hash,created_at,updated_at)
         VALUES ('sequence-user','sequence-user','sequence@example.com','',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for id in ["sequence-a", "sequence-b"] {
        sqlx::query(
            "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
             VALUES (?,'sequence-user','Sequence','acp','{}','finished',1,1)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }

    run_migrations_through(&pool, 32).await;
    let existing: Vec<(String, i64)> = sqlx::query_as(
        "SELECT conversation_id,sequence FROM conversation_memory_import_sequences
         WHERE user_id = 'sequence-user' ORDER BY sequence",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(existing.len(), 2);
    assert!(existing[0].1 < existing[1].1);
    let deleted_max = existing[1].1;

    sqlx::query("DELETE FROM conversations WHERE id = 'sequence-b'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
         VALUES ('sequence-replacement','sequence-user','Replacement','acp','{}','finished',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let replacement: i64 = sqlx::query_scalar(
        "SELECT sequence FROM conversation_memory_import_sequences
         WHERE conversation_id = 'sequence-replacement'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(replacement > deleted_max);
}

#[tokio::test]
async fn migration_032_ddl_and_backfill_are_idempotent_when_reapplied() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 31).await;
    sqlx::query(
        "INSERT INTO users (id,username,email,password_hash,created_at,updated_at)
         VALUES ('idempotent-user','idempotent-user','idempotent@example.com','',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for id in ["idempotent-a", "idempotent-b"] {
        sqlx::query(
            "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
             VALUES (?,'idempotent-user','Idempotent','acp','{}','finished',1,1)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    let migration = include_str!("../migrations/032_memory_import_sequence.sql");
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();

    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT conversation_id,sequence FROM conversation_memory_import_sequences
         WHERE user_id = 'idempotent-user' ORDER BY sequence",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].1, rows[1].1);
    let deleted_high_watermark = rows[1].1;
    let counter_before_delete: i64 =
        sqlx::query_scalar("SELECT next_sequence FROM memory_import_sequence_counter WHERE singleton = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(&rows[1].0)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    let counter_after_reapply: i64 =
        sqlx::query_scalar("SELECT next_sequence FROM memory_import_sequence_counter WHERE singleton = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counter_after_reapply, counter_before_delete);
    assert!(counter_after_reapply > deleted_high_watermark);

    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
         VALUES ('idempotent-new','idempotent-user','New','acp','{}','finished',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let new_sequence: i64 = sqlx::query_scalar(
        "SELECT sequence FROM conversation_memory_import_sequences WHERE conversation_id = 'idempotent-new'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(new_sequence, counter_after_reapply);
    assert!(new_sequence > deleted_high_watermark);
}

#[tokio::test]
async fn migration_033_adds_idempotent_immutable_retrieval_selections() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 32).await;
    sqlx::query(
        "INSERT INTO users (id,username,email,password_hash,created_at,updated_at)
         VALUES ('preview-user','preview-user','preview@example.com','',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
         VALUES ('preview-conversation','preview-user','Preview','acp','{}','finished',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_settings
            (user_id,enabled,default_capture,default_recall,consent_version,consented_at,updated_at)
         VALUES ('preview-user',1,1,1,1,1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_retrievals
            (id,user_id,conversation_id,prompt_hash,selected_ids_json,estimated_tokens,budget_tokens,
             retrieval_version,created_at,expires_at)
         VALUES ('legacy-preview','preview-user','preview-conversation','prompt','[\"legacy-entry\"]',1,100,
                 'memory-retrieval-v1',1,1000)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let migration = include_str!("../migrations/033_memory_retrieval_selections.sql");
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();

    let columns: HashSet<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('memory_retrieval_selections')")
            .fetch_all(&pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(
        columns,
        [
            "retrieval_id",
            "position",
            "selection_id",
            "selection_kind",
            "snapshot_hash"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    );

    let indexes: HashSet<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'index' AND tbl_name = 'memory_retrieval_selections'",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert!(indexes.contains("idx_memory_retrieval_selections_selection"));
    let foreign_key: (String, String, String) = sqlx::query_as(
        "SELECT \"table\",\"from\",on_delete
         FROM pragma_foreign_key_list('memory_retrieval_selections')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        foreign_key,
        ("memory_retrievals".into(), "retrieval_id".into(), "CASCADE".into()),
    );

    let repository = SqliteMemoryRepository::new(pool.clone());
    assert!(matches!(
        repository
            .consume_retrieval_snapshot(ConsumeMemoryRetrievalSnapshotRow {
                user_id: "preview-user".into(),
                conversation_id: "preview-conversation".into(),
                retrieval_id: "legacy-preview".into(),
                prompt_hash: "prompt".into(),
                retrieval_version: "memory-retrieval-v1".into(),
                expected_budget_tokens: 100,
                now: 2,
            })
            .await,
        Err(DbError::Conflict(_))
    ));

    let valid_hash = "a".repeat(64);
    sqlx::query(
        "INSERT INTO memory_retrieval_selections
            (retrieval_id,position,selection_id,selection_kind,snapshot_hash)
         VALUES ('legacy-preview',0,'legacy-entry','entry',?)",
    )
    .bind(&valid_hash)
    .execute(&pool)
    .await
    .unwrap();
    for statement in [
        "INSERT INTO memory_retrieval_selections VALUES ('legacy-preview',-1,'negative','entry','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
        "INSERT INTO memory_retrieval_selections VALUES ('legacy-preview',1,'bad-kind','other','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
        "INSERT INTO memory_retrieval_selections VALUES ('legacy-preview',1,'bad-hash','entry','short')",
        "INSERT INTO memory_retrieval_selections VALUES ('legacy-preview',0,'duplicate-position','entry','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
        "INSERT INTO memory_retrieval_selections VALUES ('legacy-preview',1,'legacy-entry','entry','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')",
    ] {
        assert!(sqlx::query(statement).execute(&pool).await.is_err(), "{statement}");
    }

    sqlx::query("DELETE FROM memory_retrievals WHERE id = 'legacy-preview'")
        .execute(&pool)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_retrieval_selections WHERE retrieval_id = 'legacy-preview'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn migration_034_scrubs_legacy_tombstones_and_is_idempotent() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    run_migrations_through(&pool, 33).await;
    sqlx::query(
        "INSERT INTO users (id,username,email,password_hash,created_at,updated_at)
         VALUES ('tombstone-user','tombstone-user','tombstone@example.com','',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,extra,status,created_at,updated_at)
         VALUES ('tombstone-conversation','tombstone-user','Tombstone','acp','{}','finished',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_entries
            (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,
             schema_version,created_at,updated_at)
         VALUES ('legacy-parent','tombstone-user','decision','parent','parent-fingerprint',
                 'parent','active',0,0,1,1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_entries
            (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,
             supersedes_id,conflict_group_id,schema_version,deleted_at,created_at,updated_at)
         VALUES ('legacy-tombstone','tombstone-user','decision','legacy secret','legacy-fingerprint',
                 NULL,'deleted',1,1,'legacy-parent','legacy-conflict',1,2,1,2)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO memory_sources
            (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
         VALUES ('legacy-tombstone','tombstone-conversation','turn','[]',1,2)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let migration = include_str!("../migrations/034_memory_tombstone_invariant.sql");
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    sqlx::raw_sql(migration).execute(&pool).await.unwrap();

    let scrubbed: (String, bool, bool, Option<String>, Option<String>, Option<String>, i64) = sqlx::query_as(
        "SELECT stable_key,pinned,user_edited,content,supersedes_id,conflict_group_id,
                (SELECT COUNT(*) FROM memory_sources WHERE memory_entry_id = memory_entries.id)
         FROM memory_entries WHERE id = 'legacy-tombstone'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(scrubbed, (String::new(), false, false, None, None, None, 0));
}

#[tokio::test]
async fn migration_031_creates_normalized_tables_constraints_and_required_indexes() {
    let database = aionui_db::init_database_memory().await.unwrap();
    let pool = database.pool();
    let expected_tables = [
        "memory_settings",
        "conversation_memory_policies",
        "conversation_memories",
        "memory_entries",
        "memory_sources",
        "memory_change_sets",
        "memory_jobs",
        "memory_job_turns",
        "memory_retrievals",
        "memory_retrieval_selections",
        "memory_import_state",
        "conversation_memory_import_sequences",
        "memory_import_sequence_counter",
    ];
    let tables: HashSet<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    for table in expected_tables {
        assert!(tables.contains(table), "missing table {table}");
    }

    let job_columns: HashSet<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('memory_jobs')")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    for column in [
        "global_epoch",
        "conversation_epoch",
        "turn_count",
        "queue_digest",
        "input_hash",
        "lease_token",
        "invalid_output_count",
        "reconciliation_snapshot_json",
    ] {
        assert!(job_columns.contains(column), "missing memory_jobs column {column}");
    }
    let entry_columns: HashSet<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('memory_entries')")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert!(
        entry_columns.contains("revision"),
        "missing memory_entries revision column"
    );

    let indexes: HashSet<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index'")
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect();
    for index in [
        "idx_messages_conversation_turn_created",
        "idx_memory_entries_user_state_scope_updated",
        "idx_memory_entries_fingerprint",
        "idx_memory_entries_one_active_fingerprint",
        "idx_memory_sources_conversation",
        "idx_memory_jobs_claim",
        "idx_memory_jobs_one_running",
        "idx_memory_jobs_one_next",
        "idx_memory_job_turns_job_position",
        "idx_memory_retrievals_expiry",
        "idx_conversation_memory_import_sequences_user",
    ] {
        assert!(indexes.contains(index), "missing index {index}");
    }
    let trigger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'trigger' AND name = 'conversations_assign_memory_import_sequence'
        )",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(trigger_exists);

    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES ('conv-constraints', 'system_default_user', 'Constraints', 'gemini', '{}', 'finished', 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();

    let invalid_tombstone = sqlx::query(
        "INSERT INTO memory_entries
         (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited, schema_version, created_at, updated_at)
         VALUES ('bad', 'system_default_user', 'decision', 'key', 'fp', 'secret', 'deleted', 0, 0, 1, 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_tombstone.is_err());

    sqlx::query(
        "INSERT INTO memory_entries
         (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited, schema_version, created_at, updated_at)
         VALUES ('active-identity', 'system_default_user', 'decision', 'key', 'shared-fp', 'active', 'active', 0, 0, 1, 1, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    let duplicate_active = sqlx::query(
        "INSERT INTO memory_entries
         (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited, schema_version, created_at, updated_at)
         VALUES ('duplicate-active', 'system_default_user', 'decision', 'key', 'shared-fp', 'duplicate', 'active', 0, 0, 1, 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(duplicate_active.is_err());
    for (id, state, stable_key, content, deleted_at) in [
        ("conflict-identity", "conflict", "key", Some("conflict"), None),
        ("deleted-identity", "deleted", "", None, Some(2_i64)),
    ] {
        sqlx::query(
            "INSERT INTO memory_entries
             (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
              schema_version, deleted_at, created_at, updated_at)
             VALUES (?, 'system_default_user', 'decision', ?, 'shared-fp', ?, ?, 0, 0, 1, ?, 1, 1)",
        )
        .bind(id)
        .bind(stable_key)
        .bind(content)
        .bind(state)
        .bind(deleted_at)
        .execute(pool)
        .await
        .unwrap();
    }

    for statement in [
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,schema_version,deleted_at,created_at,updated_at)
         VALUES ('bad-deleted-key','system_default_user','decision','secret','fp-bad-key',NULL,'deleted',0,0,1,2,1,2)",
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,schema_version,deleted_at,created_at,updated_at)
         VALUES ('bad-deleted-pinned','system_default_user','decision','','fp-bad-pinned',NULL,'deleted',1,0,1,2,1,2)",
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,schema_version,deleted_at,created_at,updated_at)
         VALUES ('bad-deleted-edited','system_default_user','decision','','fp-bad-edited',NULL,'deleted',0,1,1,2,1,2)",
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,supersedes_id,schema_version,deleted_at,created_at,updated_at)
         VALUES ('bad-deleted-supersedes','system_default_user','decision','','fp-bad-supersedes',NULL,'deleted',0,0,'active-identity',1,2,1,2)",
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,conflict_group_id,schema_version,deleted_at,created_at,updated_at)
         VALUES ('bad-deleted-conflict','system_default_user','decision','','fp-bad-conflict',NULL,'deleted',0,0,'group',1,2,1,2)",
    ] {
        assert!(sqlx::query(statement).execute(pool).await.is_err(), "{statement}");
    }
    let deleted_source = sqlx::query(
        "INSERT INTO memory_sources
            (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
         VALUES ('deleted-identity','conv-constraints','turn','[]',1,2)",
    )
    .execute(pool)
    .await;
    assert!(deleted_source.is_err());

    sqlx::query(
        "INSERT INTO memory_entries
         (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,schema_version,created_at,updated_at)
         VALUES ('transition-target','system_default_user','decision','transition-key','transition-fp',
                 'transition-content','active',0,0,1,1,1)",
    )
    .execute(pool)
    .await
    .unwrap();
    let malformed_transition = sqlx::query(
        "UPDATE memory_entries
         SET content = NULL,state = 'deleted',deleted_at = 2
         WHERE id = 'transition-target'",
    )
    .execute(pool)
    .await;
    assert!(malformed_transition.is_err());

    sqlx::query(
        "INSERT INTO memory_sources
            (memory_entry_id,conversation_id,turn_id,message_ids_json,first_observed_at,last_observed_at)
         VALUES ('active-identity','conv-constraints','active-turn','[]',1,2)",
    )
    .execute(pool)
    .await
    .unwrap();
    let sourced_transition = sqlx::query(
        "UPDATE memory_entries
         SET stable_key = '',content = NULL,state = 'deleted',pinned = 0,user_edited = 0,
             supersedes_id = NULL,conflict_group_id = NULL,deleted_at = 2
         WHERE id = 'active-identity'",
    )
    .execute(pool)
    .await;
    assert!(sourced_transition.is_err());

    let invalid_deleted_update =
        sqlx::query("UPDATE memory_entries SET conflict_group_id = 'leak' WHERE id = 'deleted-identity'")
            .execute(pool)
            .await;
    assert!(invalid_deleted_update.is_err());

    let source_move = sqlx::query(
        "UPDATE memory_sources SET memory_entry_id = 'deleted-identity'
         WHERE memory_entry_id = 'active-identity' AND conversation_id = 'conv-constraints'",
    )
    .execute(pool)
    .await;
    assert!(source_move.is_err());

    let invalid_job_state = sqlx::query(
        "INSERT INTO memory_jobs
         (id, user_id, conversation_id, through_turn_id, operation_version, global_epoch, conversation_epoch,
          turn_count, queue_digest, input_hash, expected_revision, state,
          attempt_count, created_at, updated_at)
         VALUES ('bad-job', 'system_default_user', 'conv-constraints', 'turn', 'v1', 0, 0, 0, 'digest', 'hash',
                 0, 'unknown', 0, 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_job_state.is_err());

    let invalid_reconciliation_snapshot = sqlx::query(
        "INSERT INTO memory_jobs
         (id, user_id, conversation_id, through_turn_id, operation_version, global_epoch, conversation_epoch,
          turn_count, queue_digest, input_hash, expected_revision, state, attempt_count,
          reconciliation_snapshot_json, created_at, updated_at)
         VALUES ('bad-snapshot-job', 'system_default_user', 'conv-constraints', 'turn', 'v1', 0, 0, 0,
                 'digest', 'hash', 0, 'pending', 0, '{}', 1, 1)",
    )
    .execute(pool)
    .await;
    assert!(invalid_reconciliation_snapshot.is_err());

    let object_change_arrays = sqlx::query(
        "INSERT INTO memory_change_sets
            (id, user_id, conversation_id, through_turn_id, job_id, added_ids_json, refined_ids_json,
             superseded_ids_json, conflict_ids_json, created_at)
         VALUES ('bad-change-arrays', 'system_default_user', 'conv-constraints', 'turn', 'job', '{}', '[]', '[]', '[]', 1)",
    )
    .execute(pool)
    .await;
    assert!(object_change_arrays.is_err());

    for (id, state, turn) in [
        ("running-1", "running", "turn-running-1"),
        ("pending-1", "pending", "turn-pending-1"),
    ] {
        sqlx::query(
            "INSERT INTO memory_jobs
             (id, user_id, conversation_id, through_turn_id, operation_version, global_epoch, conversation_epoch,
              turn_count, queue_digest, input_hash, expected_revision, state,
              attempt_count, created_at, updated_at)
             VALUES (?, 'system_default_user', 'conv-constraints', ?, 'v1', 0, 0, 1, ?, ?, 0, ?, 0, 1, 1)",
        )
        .bind(id)
        .bind(turn)
        .bind(format!("digest-{id}"))
        .bind(format!("hash-{id}"))
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }
    let second_running = sqlx::query(
        r#"INSERT INTO memory_jobs
         (id, user_id, conversation_id, through_turn_id, operation_version, global_epoch, conversation_epoch,
          turn_count, queue_digest, input_hash, expected_revision, state,
          attempt_count, created_at, updated_at)
         VALUES ('running-2', 'system_default_user', 'conv-constraints', 'turn-running-2', 'v1', 0, 0, 1,
                 'digest-running-2', 'hash-running-2', 0, 'running', 0, 2, 2)"#,
    )
    .execute(pool)
    .await;
    assert!(second_running.is_err());
    let second_next = sqlx::query(
        r#"INSERT INTO memory_jobs
         (id, user_id, conversation_id, through_turn_id, operation_version, global_epoch, conversation_epoch,
          turn_count, queue_digest, input_hash, expected_revision, state,
          attempt_count, created_at, updated_at)
         VALUES ('retry-2', 'system_default_user', 'conv-constraints', 'turn-retry-2', 'v1', 0, 0, 1,
                 'digest-retry-2', 'hash-retry-2', 0, 'retry_wait', 0, 2, 2)"#,
    )
    .execute(pool)
    .await;
    assert!(second_next.is_err());
}

#[test]
fn migration_versions_are_unique_and_app_operations_and_memory_own_030_through_034() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let full = Migrator::new(Path::new("migrations")).await.unwrap();
        let versions = full
            .migrations
            .iter()
            .map(|migration| migration.version)
            .collect::<Vec<_>>();
        assert_eq!(versions.iter().filter(|version| **version == 30).count(), 1);
        assert_eq!(versions.iter().filter(|version| **version == 31).count(), 1);
        assert_eq!(versions.iter().filter(|version| **version == 32).count(), 1);
        assert_eq!(versions.iter().filter(|version| **version == 33).count(), 1);
        assert_eq!(versions.iter().filter(|version| **version == 34).count(), 1);
        assert_eq!(versions.iter().copied().collect::<HashSet<_>>().len(), versions.len());
    });
}
