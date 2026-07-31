use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::Provider;
use crate::repository::IProviderRepository;
use crate::repository::provider::{CreateProviderParams, UpdateProviderParams};

/// SQLite-backed implementation of [`IProviderRepository`].
#[derive(Clone, Debug)]
pub struct SqliteProviderRepository {
    pool: SqlitePool,
}

impl SqliteProviderRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IProviderRepository for SqliteProviderRepository {
    async fn list(&self, user_id: &str) -> Result<Vec<Provider>, DbError> {
        let rows = sqlx::query_as::<_, Provider>("SELECT * FROM providers WHERE user_id = ? ORDER BY created_at ASC")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows)
    }

    async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<Provider>, DbError> {
        let row = sqlx::query_as::<_, Provider>("SELECT * FROM providers WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row)
    }

    async fn create(&self, params: CreateProviderParams<'_>) -> Result<Provider, DbError> {
        let id = params
            .id
            .map(String::from)
            .unwrap_or_else(|| aionui_common::generate_prefixed_id("prov"));
        let now = aionui_common::now_ms();

        sqlx::query(
            "INSERT INTO providers \
                (id, user_id, platform, name, base_url, api_key_encrypted, models, enabled, \
                 capabilities, context_limit, model_protocols, model_enabled, \
                 model_health, model_settings, bedrock_config, is_full_url, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.user_id)
        .bind(params.platform)
        .bind(params.name)
        .bind(params.base_url)
        .bind(params.api_key_encrypted)
        .bind(params.models)
        .bind(params.enabled)
        .bind(params.capabilities)
        .bind(params.context_limit)
        .bind(params.model_protocols)
        .bind(params.model_enabled)
        .bind(params.model_health)
        .bind(params.model_settings)
        .bind(params.bedrock_config)
        .bind(params.is_full_url)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db_err) if is_unique_violation(db_err.as_ref()) => {
                DbError::Conflict(format!("Provider with id '{id}' already exists"))
            }
            _ => DbError::Query(e),
        })?;

        Ok(Provider {
            id,
            user_id: params.user_id.to_string(),
            platform: params.platform.to_string(),
            name: params.name.to_string(),
            base_url: params.base_url.to_string(),
            api_key_encrypted: params.api_key_encrypted.to_string(),
            models: params.models.to_string(),
            enabled: params.enabled,
            capabilities: params.capabilities.to_string(),
            context_limit: params.context_limit,
            model_protocols: params.model_protocols.map(String::from),
            model_enabled: params.model_enabled.map(String::from),
            model_health: params.model_health.map(String::from),
            model_settings: params.model_settings.to_string(),
            bedrock_config: params.bedrock_config.map(String::from),
            is_full_url: params.is_full_url,
            created_at: now,
            updated_at: now,
        })
    }

    async fn update(&self, user_id: &str, id: &str, params: UpdateProviderParams<'_>) -> Result<Provider, DbError> {
        let existing = self
            .find_by_id(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Provider '{id}' not found")))?;

        let merged = merge_update(existing, params);

        sqlx::query(
            "UPDATE providers SET \
                platform = ?, name = ?, base_url = ?, api_key_encrypted = ?, \
                models = ?, enabled = ?, capabilities = ?, context_limit = ?, \
                model_protocols = ?, model_enabled = ?, model_health = ?, \
                model_settings = ?, bedrock_config = ?, is_full_url = ?, updated_at = ? \
             WHERE user_id = ? AND id = ?",
        )
        .bind(&merged.platform)
        .bind(&merged.name)
        .bind(&merged.base_url)
        .bind(&merged.api_key_encrypted)
        .bind(&merged.models)
        .bind(merged.enabled)
        .bind(&merged.capabilities)
        .bind(merged.context_limit)
        .bind(&merged.model_protocols)
        .bind(&merged.model_enabled)
        .bind(&merged.model_health)
        .bind(&merged.model_settings)
        .bind(&merged.bedrock_config)
        .bind(merged.is_full_url)
        .bind(merged.updated_at)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;

        Ok(merged)
    }

    async fn delete(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM providers WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Provider '{id}' not found")));
        }

        Ok(())
    }
}

/// Detect SQLite UNIQUE constraint violation (codes 2067 / 1555).
fn is_unique_violation(err: &dyn sqlx::error::DatabaseError) -> bool {
    err.code().is_some_and(|c| c == "2067" || c == "1555")
}

/// Merge partial update params into an existing provider, returning a new instance.
fn merge_update(existing: Provider, params: UpdateProviderParams<'_>) -> Provider {
    let now = aionui_common::now_ms();
    Provider {
        id: existing.id,
        user_id: existing.user_id,
        platform: params.platform.unwrap_or(&existing.platform).to_string(),
        name: params.name.unwrap_or(&existing.name).to_string(),
        base_url: params.base_url.unwrap_or(&existing.base_url).to_string(),
        api_key_encrypted: params
            .api_key_encrypted
            .unwrap_or(&existing.api_key_encrypted)
            .to_string(),
        models: params.models.unwrap_or(&existing.models).to_string(),
        enabled: params.enabled.unwrap_or(existing.enabled),
        capabilities: params.capabilities.unwrap_or(&existing.capabilities).to_string(),
        context_limit: params.context_limit.unwrap_or(existing.context_limit),
        model_protocols: params
            .model_protocols
            .map_or(existing.model_protocols, |v| v.map(String::from)),
        model_enabled: params
            .model_enabled
            .map_or(existing.model_enabled, |v| v.map(String::from)),
        model_health: params
            .model_health
            .map_or(existing.model_health, |v| v.map(String::from)),
        model_settings: params.model_settings.unwrap_or(&existing.model_settings).to_string(),
        bedrock_config: params
            .bedrock_config
            .map_or(existing.bedrock_config, |v| v.map(String::from)),
        is_full_url: params.is_full_url.unwrap_or(existing.is_full_url),
        created_at: existing.created_at,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "user_b";

    async fn setup() -> (SqliteProviderRepository, crate::Database) {
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
        let repo = SqliteProviderRepository::new(db.pool().clone());
        (repo, db)
    }

    fn sample_params() -> CreateProviderParams<'static> {
        CreateProviderParams {
            id: None,
            user_id: USER_A,
            platform: "anthropic",
            name: "Anthropic",
            base_url: "https://api.anthropic.com",
            api_key_encrypted: "encrypted_key_data",
            models: r#"["claude-sonnet-4-20250514"]"#,
            enabled: true,
            capabilities: r#"[{"type":"text"}]"#,
            context_limit: Some(200000),
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
        }
    }

    #[tokio::test]
    async fn list_empty() {
        let (repo, _db) = setup().await;
        let providers = repo.list(USER_A).await.unwrap();
        assert!(providers.is_empty());
    }

    #[tokio::test]
    async fn create_returns_populated_fields() {
        let (repo, _db) = setup().await;
        let p = repo.create(sample_params()).await.unwrap();

        assert!(p.id.starts_with("prov_"));
        assert_eq!(p.platform, "anthropic");
        assert_eq!(p.name, "Anthropic");
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.api_key_encrypted, "encrypted_key_data");
        assert!(p.enabled);
        assert_eq!(p.context_limit, Some(200000));
        assert!(p.model_protocols.is_none());
        assert!(p.bedrock_config.is_none());
        assert!(p.created_at > 0);
        assert_eq!(p.created_at, p.updated_at);
    }

    #[tokio::test]
    async fn create_with_caller_supplied_id() {
        let (repo, _db) = setup().await;
        let p = repo
            .create(CreateProviderParams {
                id: Some("my-custom-id-1"),
                ..sample_params()
            })
            .await
            .unwrap();

        assert_eq!(p.id, "my-custom-id-1");
        assert_eq!(p.platform, "anthropic");

        let found = repo.find_by_id(USER_A, "my-custom-id-1").await.unwrap().unwrap();
        assert_eq!(found.id, "my-custom-id-1");
    }

    #[tokio::test]
    async fn create_with_duplicate_id_returns_conflict() {
        let (repo, _db) = setup().await;
        repo.create(CreateProviderParams {
            id: Some("dup-id"),
            ..sample_params()
        })
        .await
        .unwrap();

        let err = repo
            .create(CreateProviderParams {
                id: Some("dup-id"),
                ..sample_params()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::Conflict(_)));
    }

    #[tokio::test]
    async fn create_then_find_by_id() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let found = repo.find_by_id(USER_A, &created.id).await.unwrap().unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.platform, "anthropic");
        assert_eq!(found.models, r#"["claude-sonnet-4-20250514"]"#);
    }

    #[tokio::test]
    async fn find_by_id_nonexistent() {
        let (repo, _db) = setup().await;
        assert!(repo.find_by_id(USER_A, "no_such_id").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_returns_all_ordered_by_created_at() {
        let (repo, _db) = setup().await;
        let p1 = repo.create(sample_params()).await.unwrap();
        let p2 = repo
            .create(CreateProviderParams {
                platform: "openai",
                name: "OpenAI",
                base_url: "https://api.openai.com",
                ..sample_params()
            })
            .await
            .unwrap();

        let all = repo.list(USER_A).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, p1.id);
        assert_eq!(all[1].id, p2.id);
    }

    #[tokio::test]
    async fn update_partial_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let updated = repo
            .update(
                USER_A,
                &created.id,
                UpdateProviderParams {
                    name: Some("Anthropic Updated"),
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.name, "Anthropic Updated");
        assert!(!updated.enabled);
        // Unchanged fields preserved
        assert_eq!(updated.platform, "anthropic");
        assert_eq!(updated.base_url, "https://api.anthropic.com");
        assert!(updated.updated_at >= created.updated_at);
    }

    #[tokio::test]
    async fn update_api_key() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        let updated = repo
            .update(
                USER_A,
                &created.id,
                UpdateProviderParams {
                    api_key_encrypted: Some("new_encrypted_key"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.api_key_encrypted, "new_encrypted_key");
    }

    #[tokio::test]
    async fn update_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo
            .update(USER_A, "no_id", UpdateProviderParams::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_optional_json_fields() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();
        assert!(created.model_protocols.is_none());

        // Set optional field
        let updated = repo
            .update(
                USER_A,
                &created.id,
                UpdateProviderParams {
                    model_protocols: Some(Some(r#"{"model1":"openai"}"#)),
                    bedrock_config: Some(Some(r#"{"region":"us-east-1"}"#)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.model_protocols.as_deref(), Some(r#"{"model1":"openai"}"#));
        assert_eq!(updated.bedrock_config.as_deref(), Some(r#"{"region":"us-east-1"}"#));

        // Clear optional field
        let cleared = repo
            .update(
                USER_A,
                &created.id,
                UpdateProviderParams {
                    model_protocols: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        assert!(cleared.model_protocols.is_none());
        // bedrock_config should still be set
        assert!(cleared.bedrock_config.is_some());
    }

    #[tokio::test]
    async fn delete_existing() {
        let (repo, _db) = setup().await;
        let created = repo.create(sample_params()).await.unwrap();

        repo.delete(USER_A, &created.id).await.unwrap();
        assert!(repo.find_by_id(USER_A, &created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_not_found() {
        let (repo, _db) = setup().await;
        let err = repo.delete(USER_A, "no_id").await.unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_then_list_excludes_deleted() {
        let (repo, _db) = setup().await;
        let p1 = repo.create(sample_params()).await.unwrap();
        let p2 = repo
            .create(CreateProviderParams {
                name: "Other",
                ..sample_params()
            })
            .await
            .unwrap();

        repo.delete(USER_A, &p1.id).await.unwrap();

        let all = repo.list(USER_A).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, p2.id);
    }

    #[tokio::test]
    async fn provider_operations_are_scoped_by_user() {
        let (repo, _db) = setup().await;
        let provider_a = repo.create(sample_params()).await.unwrap();
        let provider_b = repo
            .create(CreateProviderParams {
                id: Some("same-visible-provider-id"),
                user_id: USER_B,
                name: "Other User Provider",
                ..sample_params()
            })
            .await
            .unwrap();

        assert_eq!(repo.list(USER_A).await.unwrap().len(), 1);
        assert_eq!(repo.list(USER_B).await.unwrap().len(), 1);
        assert!(repo.find_by_id(USER_B, &provider_a.id).await.unwrap().is_none());

        let err = repo
            .update(
                USER_B,
                &provider_a.id,
                UpdateProviderParams {
                    name: Some("cross-user update"),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DbError::NotFound(_)));

        repo.delete(USER_B, &provider_b.id).await.unwrap();
        assert_eq!(repo.list(USER_A).await.unwrap().len(), 1);
        assert!(repo.list(USER_B).await.unwrap().is_empty());
    }
}
