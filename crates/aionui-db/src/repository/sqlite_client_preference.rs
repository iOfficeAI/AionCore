use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::ClientPreference;
use crate::repository::IClientPreferenceRepository;

/// SQLite-backed implementation of [`IClientPreferenceRepository`].
#[derive(Clone, Debug)]
pub struct SqliteClientPreferenceRepository {
    pool: SqlitePool,
}

impl SqliteClientPreferenceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IClientPreferenceRepository for SqliteClientPreferenceRepository {
    async fn get_all(&self, user_id: &str) -> Result<Vec<ClientPreference>, DbError> {
        let rows =
            sqlx::query_as::<_, ClientPreference>("SELECT * FROM client_preferences WHERE user_id = ? ORDER BY key")
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows)
    }

    async fn get_by_keys(&self, user_id: &str, keys: &[&str]) -> Result<Vec<ClientPreference>, DbError> {
        if keys.is_empty() {
            return Ok(vec![]);
        }

        // Build dynamic IN clause with positional placeholders
        let placeholders: Vec<&str> = keys.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT * FROM client_preferences WHERE user_id = ? AND key IN ({}) ORDER BY key",
            placeholders.join(", ")
        );

        let mut query = sqlx::query_as::<_, ClientPreference>(&sql).bind(user_id);
        for key in keys {
            query = query.bind(*key);
        }

        let rows = query.fetch_all(&self.pool).await?;
        Ok(rows)
    }

    async fn upsert_batch(&self, user_id: &str, entries: &[(&str, &str)]) -> Result<(), DbError> {
        if entries.is_empty() {
            return Ok(());
        }

        let now = aionui_common::now_ms();

        // Use a transaction for atomicity
        let mut tx = self.pool.begin().await?;

        for (key, value) in entries {
            sqlx::query(
                "INSERT INTO client_preferences (user_id, key, value, updated_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(user_id, key) DO UPDATE SET \
                    value = excluded.value, \
                    updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(*key)
            .bind(*value)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn delete_keys(&self, user_id: &str, keys: &[&str]) -> Result<(), DbError> {
        if keys.is_empty() {
            return Ok(());
        }

        let placeholders: Vec<&str> = keys.iter().map(|_| "?").collect();
        let sql = format!(
            "DELETE FROM client_preferences WHERE user_id = ? AND key IN ({})",
            placeholders.join(", ")
        );

        let mut query = sqlx::query(&sql).bind(user_id);
        for key in keys {
            query = query.bind(*key);
        }

        query.execute(&self.pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "user_b";

    async fn setup() -> (SqliteClientPreferenceRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
             VALUES (?, 'local', ?, 'hash', 'active', 0, 1, 1)",
        )
        .bind(USER_B)
        .bind(USER_B)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteClientPreferenceRepository::new(db.pool().clone());
        (repo, db)
    }

    #[tokio::test]
    async fn get_all_empty() {
        let (repo, _db) = setup().await;
        let prefs = repo.get_all(USER_A).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_get_all() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("theme", "\"dark\""), ("pet.size", "360")])
            .await
            .unwrap();

        let prefs = repo.get_all(USER_A).await.unwrap();
        assert_eq!(prefs.len(), 2);
        assert_eq!(prefs[0].key, "pet.size");
        assert_eq!(prefs[0].value, "360");
        assert_eq!(prefs[1].key, "theme");
        assert_eq!(prefs[1].value, "\"dark\"");
    }

    #[tokio::test]
    async fn get_by_keys_filters_correctly() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("a", "1"), ("b", "2"), ("c", "3")])
            .await
            .unwrap();

        let prefs = repo.get_by_keys(USER_A, &["a", "c", "nonexistent"]).await.unwrap();
        assert_eq!(prefs.len(), 2);

        let keys: Vec<&str> = prefs.iter().map(|p| p.key.as_str()).collect();
        assert!(keys.contains(&"a"));
        assert!(keys.contains(&"c"));
    }

    #[tokio::test]
    async fn get_by_keys_empty_input() {
        let (repo, _db) = setup().await;
        let prefs = repo.get_by_keys(USER_A, &[]).await.unwrap();
        assert!(prefs.is_empty());
    }

    #[tokio::test]
    async fn upsert_overwrites_existing_key() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("k", "v1")]).await.unwrap();
        repo.upsert_batch(USER_A, &[("k", "v2")]).await.unwrap();

        let prefs = repo.get_all(USER_A).await.unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].value, "v2");
    }

    #[tokio::test]
    async fn delete_keys_removes_entries() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("a", "1"), ("b", "2"), ("c", "3")])
            .await
            .unwrap();

        repo.delete_keys(USER_A, &["a", "c"]).await.unwrap();

        let prefs = repo.get_all(USER_A).await.unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].key, "b");
    }

    #[tokio::test]
    async fn delete_keys_nonexistent_is_noop() {
        let (repo, _db) = setup().await;
        repo.delete_keys(USER_A, &["ghost"]).await.unwrap();
        assert!(repo.get_all(USER_A).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_keys_empty_input() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("x", "1")]).await.unwrap();
        repo.delete_keys(USER_A, &[]).await.unwrap();
        assert_eq!(repo.get_all(USER_A).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn upsert_empty_batch_is_noop() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[]).await.unwrap();
        assert!(repo.get_all(USER_A).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn preferences_are_scoped_by_user() {
        let (repo, _db) = setup().await;
        repo.upsert_batch(USER_A, &[("theme", "\"dark\"")]).await.unwrap();
        repo.upsert_batch(USER_B, &[("theme", "\"light\"")]).await.unwrap();

        assert_eq!(repo.get_by_keys(USER_A, &["theme"]).await.unwrap()[0].value, "\"dark\"");
        assert_eq!(
            repo.get_by_keys(USER_B, &["theme"]).await.unwrap()[0].value,
            "\"light\""
        );

        repo.delete_keys(USER_B, &["theme"]).await.unwrap();
        assert!(repo.get_by_keys(USER_B, &["theme"]).await.unwrap().is_empty());
        assert_eq!(repo.get_by_keys(USER_A, &["theme"]).await.unwrap().len(), 1);
    }
}
