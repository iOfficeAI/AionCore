use std::collections::HashMap;
use std::sync::Arc;

use aionui_api_types::{
    AppOperationsModelHealth, AppOperationsModelReasonCode, AppOperationsModelRef, AppOperationsModelResponse,
    AppOperationsModelSetting, HealthStatus, ModelCapability, ModelHealthStatus, ModelType, SystemSettingsResponse,
    UpdateAppOperationsModelRequest, UpdateSettingsRequest,
};
use aionui_db::models::Provider;
use aionui_db::{IProviderRepository, ISettingsRepository, UpdateProviderParams};
use tracing::info;

use crate::error::SystemError;

/// Supported BCP 47 language codes.
const SUPPORTED_LANGUAGES: &[&str] = &[
    "en-US", "zh-CN", "zh-TW", "ja-JP", "ko-KR", "fr-FR", "de-DE", "es-ES", "pt-BR", "ru-RU", "ar-SA", "it-IT",
    "nl-NL", "pl-PL", "tr-TR", "vi-VN", "th-TH", "id-ID",
];

/// Business logic for system settings (language, notifications, etc.).
#[derive(Clone)]
pub struct SettingsService {
    repo: Arc<dyn ISettingsRepository>,
    provider_repo: Option<Arc<dyn IProviderRepository>>,
}

impl SettingsService {
    pub fn new(repo: Arc<dyn ISettingsRepository>) -> Self {
        Self {
            repo,
            provider_repo: None,
        }
    }

    pub fn with_provider_repo(mut self, provider_repo: Arc<dyn IProviderRepository>) -> Self {
        self.provider_repo = Some(provider_repo);
        self
    }

    /// Get current system settings, falling back to defaults if not yet persisted.
    pub async fn get_settings(&self) -> Result<SystemSettingsResponse, SystemError> {
        let row = self
            .repo
            .get_settings()
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to get settings: {e}")))?;

        Ok(
            row.map_or_else(SystemSettingsResponse::default, |s| SystemSettingsResponse {
                language: s.language,
                notification_enabled: s.notification_enabled,
                cron_notification_enabled: s.cron_notification_enabled,
                command_queue_enabled: s.command_queue_enabled,
                save_upload_to_workspace: s.save_upload_to_workspace,
            }),
        )
    }

    /// Partially update system settings. Only fields present in the request are changed.
    pub async fn update_settings(&self, req: UpdateSettingsRequest) -> Result<SystemSettingsResponse, SystemError> {
        if let Some(ref lang) = req.language {
            validate_language(lang)?;
        }

        // Merge with current settings (or defaults)
        let current = self.get_settings().await?;

        let language = req.language.unwrap_or(current.language);
        let notification_enabled = req.notification_enabled.unwrap_or(current.notification_enabled);
        let cron_notification_enabled = req
            .cron_notification_enabled
            .unwrap_or(current.cron_notification_enabled);
        let command_queue_enabled = req.command_queue_enabled.unwrap_or(current.command_queue_enabled);
        let save_upload_to_workspace = req.save_upload_to_workspace.unwrap_or(current.save_upload_to_workspace);

        let row = self
            .repo
            .upsert_settings(
                &language,
                notification_enabled,
                cron_notification_enabled,
                command_queue_enabled,
                save_upload_to_workspace,
            )
            .await
            .map_err(|e| SystemError::Internal(format!("Failed to update settings: {e}")))?;

        Ok(SystemSettingsResponse {
            language: row.language,
            notification_enabled: row.notification_enabled,
            cron_notification_enabled: row.cron_notification_enabled,
            command_queue_enabled: row.command_queue_enabled,
            save_upload_to_workspace: row.save_upload_to_workspace,
        })
    }

    pub async fn get_app_operations_model(&self) -> Result<AppOperationsModelResponse, SystemError> {
        self.provider_repo()?;
        let stored = self.repo.get_app_operations_model().await?;
        let setting = match stored.mode.as_str() {
            "auto" => AppOperationsModelSetting::Auto,
            "fixed" => AppOperationsModelSetting::Fixed {
                provider_id: stored.provider_id.ok_or_else(|| {
                    SystemError::Internal("Fixed App Operations setting is missing provider id".into())
                })?,
                model_id: stored
                    .model_id
                    .ok_or_else(|| SystemError::Internal("Fixed App Operations setting is missing model id".into()))?,
            },
            _ => {
                return Err(SystemError::Internal(
                    "Invalid App Operations model setting mode".into(),
                ));
            }
        };

        self.resolve_app_operations_model(setting).await
    }

    pub async fn update_app_operations_model(
        &self,
        request: UpdateAppOperationsModelRequest,
    ) -> Result<AppOperationsModelResponse, SystemError> {
        let provider_repo = self.provider_repo()?;
        let setting = match request {
            AppOperationsModelSetting::Auto => AppOperationsModelSetting::Auto,
            AppOperationsModelSetting::Fixed { provider_id, model_id } => {
                let provider_id = provider_id.trim().to_owned();
                let model_id = model_id.trim().to_owned();
                if provider_id.is_empty() || model_id.is_empty() {
                    return Err(SystemError::UnprocessableEntity(
                        "Fixed App Operations provider and model ids must not be empty".into(),
                    ));
                }

                let provider = provider_repo
                    .find_by_id(&provider_id)
                    .await?
                    .ok_or_else(|| SystemError::UnprocessableEntity("App Operations provider does not exist".into()))?;
                let models = provider_models(&provider)?;
                if !models.iter().any(|stored_model_id| stored_model_id == &model_id) {
                    return Err(SystemError::UnprocessableEntity(
                        "App Operations model does not exist for the selected provider".into(),
                    ));
                }

                AppOperationsModelSetting::Fixed { provider_id, model_id }
            }
        };

        let (mode, provider_id, model_id) = match &setting {
            AppOperationsModelSetting::Auto => ("auto", None, None),
            AppOperationsModelSetting::Fixed { provider_id, model_id } => {
                ("fixed", Some(provider_id.as_str()), Some(model_id.as_str()))
            }
        };
        self.repo
            .upsert_app_operations_model(mode, provider_id, model_id)
            .await?;
        info!(
            mode,
            provider_id = provider_id.unwrap_or(""),
            model_id = model_id.unwrap_or(""),
            "App Operations model setting updated"
        );

        self.resolve_app_operations_model(setting).await
    }

    pub async fn record_app_operations_health(
        &self,
        provider_id: &str,
        model_id: &str,
        status: HealthStatus,
        checked_at: i64,
        latency_ms: i64,
    ) -> Result<(), SystemError> {
        let provider_repo = self.provider_repo()?;
        let provider = provider_repo
            .find_by_id(provider_id)
            .await?
            .ok_or_else(|| SystemError::UnprocessableEntity("App Operations provider does not exist".into()))?;
        let mut health = provider_health_map(&provider)?;
        health.insert(
            model_id.to_owned(),
            ModelHealthStatus {
                status,
                last_check: Some(checked_at),
                latency: Some(latency_ms),
                error: None,
            },
        );
        let serialized = serde_json::to_string(&health)
            .map_err(|_| SystemError::Internal("Failed to serialize provider model health".into()))?;

        provider_repo
            .update(
                provider_id,
                UpdateProviderParams {
                    model_health: Some(Some(serialized.as_str())),
                    ..Default::default()
                },
            )
            .await?;
        Ok(())
    }

    fn provider_repo(&self) -> Result<&Arc<dyn IProviderRepository>, SystemError> {
        self.provider_repo
            .as_ref()
            .ok_or_else(|| SystemError::Internal("App Operations provider repository is not configured".into()))
    }

    async fn resolve_app_operations_model(
        &self,
        setting: AppOperationsModelSetting,
    ) -> Result<AppOperationsModelResponse, SystemError> {
        match &setting {
            AppOperationsModelSetting::Auto => self.resolve_auto(setting).await,
            AppOperationsModelSetting::Fixed { provider_id, model_id } => {
                self.resolve_fixed(setting.clone(), provider_id.as_str(), model_id.as_str())
                    .await
            }
        }
    }

    async fn resolve_auto(
        &self,
        setting: AppOperationsModelSetting,
    ) -> Result<AppOperationsModelResponse, SystemError> {
        for provider in self.provider_repo()?.list().await? {
            if !provider.enabled || !provider_has_auth(&provider) || !provider_supports_text(&provider)? {
                continue;
            }

            for model_id in provider_models(&provider)? {
                if model_id.trim().is_empty() || !model_is_enabled(&provider, &model_id)? {
                    continue;
                }
                let health = model_health(&provider, &model_id)?;
                if health
                    .as_ref()
                    .is_some_and(|value| value.status == HealthStatus::Unhealthy)
                {
                    continue;
                }

                return Ok(ready_response(
                    setting,
                    provider.id,
                    model_id,
                    health.and_then(|value| value.last_check),
                ));
            }
        }

        Ok(AppOperationsModelResponse {
            setting,
            resolved_model: None,
            health: AppOperationsModelHealth::SetupRequired,
            reason_code: Some(AppOperationsModelReasonCode::NoEligibleModel),
            checked_at: None,
        })
    }

    async fn resolve_fixed(
        &self,
        setting: AppOperationsModelSetting,
        provider_id: &str,
        model_id: &str,
    ) -> Result<AppOperationsModelResponse, SystemError> {
        let Some(provider) = self.provider_repo()?.find_by_id(provider_id).await? else {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::ProviderMissing,
                None,
            ));
        };
        if !provider.enabled {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::ProviderDisabled,
                None,
            ));
        }
        if !provider_models(&provider)?
            .iter()
            .any(|stored_model_id| stored_model_id == model_id)
        {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::ModelMissing,
                None,
            ));
        }
        if !model_is_enabled(&provider, model_id)? {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::ModelDisabled,
                None,
            ));
        }
        if !provider_has_auth(&provider) {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::AuthRequired,
                None,
            ));
        }

        let health = model_health(&provider, model_id)?;
        let checked_at = health.as_ref().and_then(|value| value.last_check);
        if health.is_some_and(|value| value.status == HealthStatus::Unhealthy) {
            return Ok(unavailable_response(
                setting,
                AppOperationsModelReasonCode::HealthCheckFailed,
                checked_at,
            ));
        }

        Ok(ready_response(setting, provider.id, model_id.to_owned(), checked_at))
    }
}

fn provider_models(provider: &Provider) -> Result<Vec<String>, SystemError> {
    serde_json::from_str(&provider.models).map_err(|_| SystemError::Internal("Failed to parse provider models".into()))
}

fn provider_has_auth(provider: &Provider) -> bool {
    !provider.api_key_encrypted.trim().is_empty()
        || (provider.platform == "bedrock" && provider.bedrock_config.is_some())
}

fn model_is_enabled(provider: &Provider, model_id: &str) -> Result<bool, SystemError> {
    let Some(serialized) = &provider.model_enabled else {
        return Ok(true);
    };
    let enabled: HashMap<String, bool> = serde_json::from_str(serialized)
        .map_err(|_| SystemError::Internal("Failed to parse provider model enablement".into()))?;
    Ok(enabled.get(model_id).copied().unwrap_or(true))
}

fn provider_health_map(provider: &Provider) -> Result<HashMap<String, ModelHealthStatus>, SystemError> {
    let Some(serialized) = &provider.model_health else {
        return Ok(HashMap::new());
    };
    serde_json::from_str(serialized).map_err(|_| SystemError::Internal("Failed to parse provider model health".into()))
}

fn model_health(provider: &Provider, model_id: &str) -> Result<Option<ModelHealthStatus>, SystemError> {
    Ok(provider_health_map(provider)?.remove(model_id))
}

fn provider_supports_text(provider: &Provider) -> Result<bool, SystemError> {
    let capabilities: Vec<ModelCapability> = serde_json::from_str(&provider.capabilities)
        .map_err(|_| SystemError::Internal("Failed to parse provider capabilities".into()))?;
    if capabilities.is_empty() {
        return Ok(true);
    }
    if capabilities.iter().any(|capability| {
        capability.capability_type == ModelType::ExcludeFromPrimary && capability.is_user_selected != Some(false)
    }) {
        return Ok(false);
    }
    Ok(capabilities
        .iter()
        .any(|capability| capability.capability_type == ModelType::Text))
}

fn ready_response(
    setting: AppOperationsModelSetting,
    provider_id: String,
    model_id: String,
    checked_at: Option<i64>,
) -> AppOperationsModelResponse {
    AppOperationsModelResponse {
        setting,
        resolved_model: Some(AppOperationsModelRef { provider_id, model_id }),
        health: AppOperationsModelHealth::Ready,
        reason_code: None,
        checked_at,
    }
}

fn unavailable_response(
    setting: AppOperationsModelSetting,
    reason_code: AppOperationsModelReasonCode,
    checked_at: Option<i64>,
) -> AppOperationsModelResponse {
    AppOperationsModelResponse {
        setting,
        resolved_model: None,
        health: AppOperationsModelHealth::Unavailable,
        reason_code: Some(reason_code),
        checked_at,
    }
}

fn validate_language(lang: &str) -> Result<(), SystemError> {
    if SUPPORTED_LANGUAGES.contains(&lang) {
        Ok(())
    } else {
        Err(SystemError::BadRequest(format!("Unsupported language code: '{lang}'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteSettingsRepository, init_database_memory};

    async fn setup() -> SettingsService {
        let db = init_database_memory().await.unwrap();
        let repo = Arc::new(SqliteSettingsRepository::new(db.pool().clone()));
        // Leak the db handle so the pool stays alive for the test
        std::mem::forget(db);
        SettingsService::new(repo)
    }

    #[test]
    fn validate_language_accepts_supported() {
        assert!(validate_language("en-US").is_ok());
        assert!(validate_language("zh-CN").is_ok());
        assert!(validate_language("ja-JP").is_ok());
    }

    #[test]
    fn validate_language_rejects_unsupported() {
        assert!(validate_language("invalid").is_err());
        assert!(validate_language("").is_err());
        assert!(validate_language("xx-YY").is_err());
    }

    #[tokio::test]
    async fn get_settings_returns_defaults_when_empty() {
        let svc = setup().await;
        let settings = svc.get_settings().await.unwrap();
        assert_eq!(settings, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_single_field() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("zh-CN".into()),
            ..Default::default()
        };
        let result = svc.update_settings(req).await.unwrap();
        assert_eq!(result.language, "zh-CN");
        // Other fields stay at defaults
        assert!(result.notification_enabled);
        assert!(!result.cron_notification_enabled);
    }

    #[tokio::test]
    async fn update_multiple_fields() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            notification_enabled: Some(false),
            command_queue_enabled: Some(true),
            ..Default::default()
        };
        let result = svc.update_settings(req).await.unwrap();
        assert!(!result.notification_enabled);
        assert!(result.command_queue_enabled);
        assert_eq!(result.language, "en-US");
    }

    #[tokio::test]
    async fn update_empty_request_returns_current() {
        let svc = setup().await;
        let result = svc.update_settings(UpdateSettingsRequest::default()).await.unwrap();
        assert_eq!(result, SystemSettingsResponse::default());
    }

    #[tokio::test]
    async fn update_invalid_language_rejected() {
        let svc = setup().await;
        let req = UpdateSettingsRequest {
            language: Some("invalid-lang".into()),
            ..Default::default()
        };
        let err = svc.update_settings(req).await.unwrap_err();
        assert!(matches!(err, SystemError::BadRequest(_)));
    }

    #[tokio::test]
    async fn update_then_get_reflects_changes() {
        let svc = setup().await;
        svc.update_settings(UpdateSettingsRequest {
            language: Some("ja-JP".into()),
            save_upload_to_workspace: Some(true),
            ..Default::default()
        })
        .await
        .unwrap();

        let settings = svc.get_settings().await.unwrap();
        assert_eq!(settings.language, "ja-JP");
        assert!(settings.save_upload_to_workspace);
    }

    mod app_operations {
        use super::*;
        use aionui_db::{CreateProviderParams, IProviderRepository, SqliteProviderRepository, UpdateProviderParams};

        async fn setup_app_operations() -> (
            SettingsService,
            Arc<SqliteSettingsRepository>,
            Arc<SqliteProviderRepository>,
        ) {
            let db = init_database_memory().await.unwrap();
            let settings_repo = Arc::new(SqliteSettingsRepository::new(db.pool().clone()));
            let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
            std::mem::forget(db);

            let service = SettingsService::new(settings_repo.clone()).with_provider_repo(provider_repo.clone());
            (service, settings_repo, provider_repo)
        }

        async fn create_provider(
            repo: &SqliteProviderRepository,
            id: &str,
            models: &str,
            enabled: bool,
            model_enabled: Option<&str>,
            model_health: Option<&str>,
        ) {
            repo.create(CreateProviderParams {
                id: Some(id),
                platform: "openai",
                name: id,
                base_url: "https://example.invalid/v1",
                api_key_encrypted: "non-secret-encrypted-test-value",
                models,
                enabled,
                capabilities: r#"[{"type":"text"}]"#,
                context_limit: None,
                model_protocols: None,
                model_enabled,
                model_health,
                model_settings: "{}",
                bedrock_config: None,
                is_full_url: false,
            })
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn auto_uses_first_eligible_provider_and_model_in_repository_order() {
            let (service, _, provider_repo) = setup_app_operations().await;
            create_provider(
                &provider_repo,
                "provider-1",
                r#"["model-a","model-c"]"#,
                true,
                None,
                None,
            )
            .await;
            create_provider(&provider_repo, "provider-2", r#"["model-b"]"#, true, None, None).await;

            let response = service.get_app_operations_model().await.unwrap();

            assert_eq!(response.setting, AppOperationsModelSetting::Auto);
            assert_eq!(response.health, AppOperationsModelHealth::Ready);
            let resolved = response.resolved_model.unwrap();
            assert_eq!(resolved.provider_id, "provider-1");
            assert_eq!(resolved.model_id, "model-a");
            assert_eq!(response.reason_code, None);
        }

        #[tokio::test]
        async fn auto_skips_disabled_provider_disabled_model_and_unhealthy_model() {
            let (service, _, provider_repo) = setup_app_operations().await;
            create_provider(&provider_repo, "provider-disabled", r#"["model-z"]"#, false, None, None).await;
            create_provider(
                &provider_repo,
                "provider-1",
                r#"["model-disabled","model-unhealthy"]"#,
                true,
                Some(r#"{"model-disabled":false}"#),
                Some(r#"{"model-unhealthy":{"status":"unhealthy"}}"#),
            )
            .await;
            create_provider(&provider_repo, "provider-2", r#"["model-b"]"#, true, None, None).await;

            let response = service.get_app_operations_model().await.unwrap();

            assert_eq!(response.health, AppOperationsModelHealth::Ready);
            let resolved = response.resolved_model.unwrap();
            assert_eq!(resolved.provider_id, "provider-2");
            assert_eq!(resolved.model_id, "model-b");
            assert_eq!(response.reason_code, None);
        }

        #[tokio::test]
        async fn auto_without_candidate_returns_setup_required() {
            let (service, _, _) = setup_app_operations().await;

            let response = service.get_app_operations_model().await.unwrap();

            assert_eq!(response.setting, AppOperationsModelSetting::Auto);
            assert_eq!(response.resolved_model, None);
            assert_eq!(response.health, AppOperationsModelHealth::SetupRequired);
            assert_eq!(
                response.reason_code,
                Some(AppOperationsModelReasonCode::NoEligibleModel)
            );
        }

        #[tokio::test]
        async fn fixed_never_substitutes_when_provider_becomes_disabled() {
            let (service, settings_repo, provider_repo) = setup_app_operations().await;
            create_provider(&provider_repo, "provider-1", r#"["model-a"]"#, true, None, None).await;
            create_provider(&provider_repo, "provider-2", r#"["model-b"]"#, true, None, None).await;
            service
                .update_app_operations_model(AppOperationsModelSetting::Fixed {
                    provider_id: "provider-1".into(),
                    model_id: "model-a".into(),
                })
                .await
                .unwrap();
            provider_repo
                .update(
                    "provider-1",
                    UpdateProviderParams {
                        enabled: Some(false),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            let response = service.get_app_operations_model().await.unwrap();

            assert_eq!(
                response.setting,
                AppOperationsModelSetting::Fixed {
                    provider_id: "provider-1".into(),
                    model_id: "model-a".into(),
                }
            );
            assert_eq!(response.resolved_model, None);
            assert_eq!(response.health, AppOperationsModelHealth::Unavailable);
            assert_eq!(
                response.reason_code,
                Some(AppOperationsModelReasonCode::ProviderDisabled)
            );
            let stored = settings_repo.get_app_operations_model().await.unwrap();
            assert_eq!(stored.mode, "fixed");
            assert_eq!(stored.provider_id.as_deref(), Some("provider-1"));
            assert_eq!(stored.model_id.as_deref(), Some("model-a"));
        }

        #[tokio::test]
        async fn fixed_update_rejects_unknown_provider_or_model() {
            let (service, settings_repo, provider_repo) = setup_app_operations().await;
            create_provider(&provider_repo, "provider-1", r#"["model-a"]"#, true, None, None).await;

            let unknown_provider = service
                .update_app_operations_model(AppOperationsModelSetting::Fixed {
                    provider_id: "unknown-provider".into(),
                    model_id: "model-a".into(),
                })
                .await
                .unwrap_err();
            assert!(matches!(unknown_provider, SystemError::UnprocessableEntity(_)));
            let stored = settings_repo.get_app_operations_model().await.unwrap();
            assert_eq!(stored.mode, "auto");
            assert_eq!(stored.provider_id, None);
            assert_eq!(stored.model_id, None);

            let unknown_model = service
                .update_app_operations_model(AppOperationsModelSetting::Fixed {
                    provider_id: "provider-1".into(),
                    model_id: "unknown-model".into(),
                })
                .await
                .unwrap_err();
            assert!(matches!(unknown_model, SystemError::UnprocessableEntity(_)));
            let stored = settings_repo.get_app_operations_model().await.unwrap();
            assert_eq!(stored.mode, "auto");
            assert_eq!(stored.provider_id, None);
            assert_eq!(stored.model_id, None);
        }

        #[tokio::test]
        async fn record_app_operations_health_updates_only_selected_model_without_error_text() {
            let (service, _, provider_repo) = setup_app_operations().await;
            create_provider(
                &provider_repo,
                "provider-1",
                r#"["model-a","model-b"]"#,
                true,
                None,
                Some(r#"{"model-b":{"status":"healthy","last_check":10,"latency":20,"error":"existing"}}"#),
            )
            .await;

            service
                .record_app_operations_health("provider-1", "model-a", HealthStatus::Unhealthy, 123, 45)
                .await
                .unwrap();

            let provider = provider_repo.find_by_id("provider-1").await.unwrap().unwrap();
            let health: HashMap<String, ModelHealthStatus> =
                serde_json::from_str(provider.model_health.as_deref().unwrap()).unwrap();
            assert_eq!(health["model-a"].status, HealthStatus::Unhealthy);
            assert_eq!(health["model-a"].last_check, Some(123));
            assert_eq!(health["model-a"].latency, Some(45));
            assert_eq!(health["model-a"].error, None);
            assert_eq!(health["model-b"].status, HealthStatus::Healthy);
            assert_eq!(health["model-b"].error.as_deref(), Some("existing"));
        }

        #[tokio::test]
        async fn app_operations_methods_require_provider_repository() {
            let service = setup().await;

            let error = service.get_app_operations_model().await.unwrap_err();

            assert!(matches!(
                error,
                SystemError::Internal(ref message)
                    if message == "App Operations provider repository is not configured"
            ));
        }
    }
}
