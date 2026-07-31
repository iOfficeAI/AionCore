use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use aionui_common::{CommandSpec, EnvVar, ProviderWithModel};
use aionui_db::IProviderRepository;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::capability::cli_process::MANAGED_HERMES_ENV_ISOLATION_MARKER;
use crate::error::AgentError;
use crate::factory::aionrs::map_aionrs_provider;

const HERMES_CONFIG: &[u8] = b"security:\n  allow_lazy_installs: false\n\
auxiliary:\n  title_generation:\n    enabled: false\n";

struct ResolvedHermesProvider {
    base_url: String,
    api_key: String,
    model: String,
}

struct HermesSessionOverlay {
    env: Vec<EnvVar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HermesLaunchPolicy {
    NotHermes,
    External,
    Managed,
}

pub(super) async fn apply_if_hermes(
    command_spec: &mut CommandSpec,
    launch_policy: HermesLaunchPolicy,
    model: &ProviderWithModel,
    provider_repo: &dyn IProviderRepository,
    encryption_key: &[u8; 32],
    data_dir: &Path,
    conversation_id: &str,
) -> Result<(), AgentError> {
    if launch_policy != HermesLaunchPolicy::Managed {
        return Ok(());
    }

    let overlay = build_session_overlay(model, provider_repo, encryption_key, data_dir, conversation_id).await?;
    apply_env_overlay(&mut command_spec.env, overlay.env);
    apply_env_overlay(
        &mut command_spec.env,
        vec![env_var(MANAGED_HERMES_ENV_ISOLATION_MARKER, "1")],
    );
    Ok(())
}

async fn build_session_overlay(
    model: &ProviderWithModel,
    provider_repo: &dyn IProviderRepository,
    encryption_key: &[u8; 32],
    data_dir: &Path,
    conversation_id: &str,
) -> Result<HermesSessionOverlay, AgentError> {
    let provider = resolve_provider(model, provider_repo, encryption_key).await?;
    let home = session_home(data_dir, conversation_id);
    ensure_minimal_config(&home).await?;
    let home_value = home
        .to_str()
        .ok_or_else(|| AgentError::internal("Hermes session home is not valid UTF-8"))?
        .to_owned();

    Ok(HermesSessionOverlay {
        env: vec![
            env_var("OPENAI_BASE_URL", provider.base_url),
            env_var("OPENAI_API_KEY", provider.api_key),
            env_var("HERMES_INFERENCE_MODEL", provider.model),
            env_var("HERMES_HOME", home_value),
            env_var("HERMES_DISABLE_LAZY_INSTALLS", "1"),
            env_var("HERMES_ACP_SKIP_CONFIGURED_MCP", "1"),
            env_var("HERMES_ACP_TOOLSET", "hermes-acp-lite"),
        ],
    })
}

async fn resolve_provider(
    model: &ProviderWithModel,
    provider_repo: &dyn IProviderRepository,
    encryption_key: &[u8; 32],
) -> Result<ResolvedHermesProvider, AgentError> {
    let provider_id = model.provider_id.trim();
    if provider_id.is_empty() {
        return Err(AgentError::bad_request("Hermes requires a model provider"));
    }

    let model_id = effective_model(model)?;
    let row = provider_repo
        .find_by_id(provider_id)
        .await
        .map_err(|error| AgentError::internal(format!("Failed to load Hermes provider config: {error}")))?
        .ok_or_else(|| AgentError::bad_request(format!("Provider '{provider_id}' not found")))?;

    if !row.enabled {
        return Err(AgentError::bad_request(format!("Provider '{provider_id}' is disabled")));
    }
    if row.is_full_url {
        return Err(AgentError::bad_request(
            "Hermes beta does not support providers configured with a full endpoint URL",
        ));
    }

    let base_url = row.base_url.trim();
    if base_url.is_empty() {
        return Err(AgentError::bad_request(format!(
            "Provider '{provider_id}' has an empty base URL"
        )));
    }

    let transport = map_aionrs_provider(&row.platform, &model_id, row.model_protocols.as_deref())?;
    if transport != "openai" {
        return Err(AgentError::bad_request(format!(
            "Hermes beta requires an OpenAI-compatible provider; provider '{provider_id}' resolves to '{transport}'"
        )));
    }

    let api_key = aionui_common::decrypt_string(&row.api_key_encrypted, encryption_key).map_err(|error| {
        AgentError::internal(format!(
            "Failed to decrypt API key for Hermes provider '{provider_id}': {error}"
        ))
    })?;

    Ok(ResolvedHermesProvider {
        base_url: base_url.to_owned(),
        api_key,
        model: model_id,
    })
}

fn effective_model(model: &ProviderWithModel) -> Result<String, AgentError> {
    let model_id = model
        .use_model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| model.model.trim());
    if model_id.is_empty() {
        return Err(AgentError::bad_request("Hermes requires a non-empty model"));
    }
    Ok(model_id.to_owned())
}

fn session_home(data_dir: &Path, conversation_id: &str) -> PathBuf {
    let digest = Sha256::digest(conversation_id.as_bytes());
    data_dir.join("hermes-sessions").join(hex::encode(digest))
}

async fn ensure_minimal_config(home: &Path) -> Result<PathBuf, AgentError> {
    tokio::fs::create_dir_all(home)
        .await
        .map_err(|error| AgentError::internal(format!("Failed to create Hermes session home: {error}")))?;
    let config_path = home.join("config.yaml");
    if file_matches(&config_path, HERMES_CONFIG).await {
        return Ok(config_path);
    }

    atomic_write(&config_path, HERMES_CONFIG)
        .await
        .map_err(|error| AgentError::internal(format!("Failed to write Hermes session config: {error}")))?;
    Ok(config_path)
}

async fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "Hermes config path has no parent"))?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("config.yaml");
    let temp_path = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::now_v7()
    ));

    let write_result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .await?;
        file.write_all(contents).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        commit_temp_file(&temp_path, path, contents).await
    }
    .await;

    let _ = tokio::fs::remove_file(&temp_path).await;
    write_result
}

#[cfg(not(windows))]
async fn commit_temp_file(temp_path: &Path, path: &Path, _contents: &[u8]) -> std::io::Result<()> {
    tokio::fs::rename(temp_path, path).await
}

#[cfg(windows)]
async fn commit_temp_file(temp_path: &Path, path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if file_matches(path, contents).await {
        return Ok(());
    }

    let result = move_file_replace(temp_path.to_owned(), path.to_owned()).await;
    if result.is_err() && file_matches(path, contents).await {
        Ok(())
    } else {
        result
    }
}

#[cfg(windows)]
async fn move_file_replace(source: PathBuf, destination: PathBuf) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || move_file_replace_blocking(&source, &destination))
        .await
        .map_err(|error| std::io::Error::other(format!("Hermes config replace task failed: {error}")))?
}

#[cfg(windows)]
fn move_file_replace_blocking(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "MoveFileExW"]
        fn move_file_ex_w(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
    }

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both arguments are valid, NUL-terminated UTF-16 paths for the
    // duration of the call. The files share a directory, so this remains a
    // same-volume atomic replacement.
    let replaced = unsafe {
        move_file_ex_w(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn file_matches(path: &Path, expected: &[u8]) -> bool {
    tokio::fs::read(path).await.is_ok_and(|contents| contents == expected)
}

fn apply_env_overlay(existing: &mut Vec<EnvVar>, overlay: Vec<EnvVar>) {
    for entry in overlay {
        existing.retain(|current| !current.name.eq_ignore_ascii_case(&entry.name));
        existing.push(entry);
    }
}

fn env_var(name: impl Into<String>, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: value.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aionui_common::encrypt_string;
    use aionui_db::{CreateProviderParams, SqliteProviderRepository, init_database_memory};

    use super::*;

    const TEST_KEY: [u8; 32] = [0x42; 32];

    fn selected_model(provider_id: &str, model: &str, use_model: Option<&str>) -> ProviderWithModel {
        ProviderWithModel {
            provider_id: provider_id.to_owned(),
            model: model.to_owned(),
            use_model: use_model.map(str::to_owned),
        }
    }

    async fn insert_provider(
        repo: &dyn IProviderRepository,
        id: &str,
        platform: &str,
        base_url: &str,
        api_key: &str,
        model_protocols: Option<&str>,
        is_full_url: bool,
    ) {
        let encrypted = encrypt_string(api_key, &TEST_KEY).unwrap();
        repo.create(CreateProviderParams {
            id: Some(id),
            platform,
            name: id,
            base_url,
            api_key_encrypted: &encrypted,
            models: r#"["model-a","model-b"]"#,
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url,
        })
        .await
        .unwrap();
    }

    #[test]
    fn effective_model_prefers_non_empty_use_model() {
        assert_eq!(
            effective_model(&selected_model("provider", "default-model", Some("override-model"))).unwrap(),
            "override-model"
        );
        assert_eq!(
            effective_model(&selected_model("provider", "default-model", Some(""))).unwrap(),
            "default-model"
        );
        assert!(effective_model(&selected_model("provider", "  ", Some(" "))).is_err());
    }

    #[tokio::test]
    async fn rejects_unsupported_protocol_and_full_url() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        insert_provider(
            repo.as_ref(),
            "anthropic-provider",
            "custom",
            "https://anthropic.example",
            "secret-a",
            Some(r#"{"model-a":"anthropic"}"#),
            false,
        )
        .await;
        insert_provider(
            repo.as_ref(),
            "full-url-provider",
            "openai",
            "https://openai.example/v1/chat/completions",
            "secret-b",
            None,
            true,
        )
        .await;

        let unsupported = resolve_provider(
            &selected_model("anthropic-provider", "model-a", None),
            repo.as_ref(),
            &TEST_KEY,
        )
        .await
        .err()
        .expect("unsupported provider must fail");
        assert!(unsupported.to_string().contains("OpenAI-compatible"));

        let full_url = resolve_provider(
            &selected_model("full-url-provider", "model-a", None),
            repo.as_ref(),
            &TEST_KEY,
        )
        .await
        .err()
        .expect("full endpoint URL must fail");
        assert!(full_url.to_string().contains("full endpoint URL"));
    }

    #[tokio::test]
    async fn reports_missing_provider_empty_base_and_decrypt_failure() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));

        let missing = resolve_provider(
            &selected_model("missing-provider", "model-a", None),
            repo.as_ref(),
            &TEST_KEY,
        )
        .await
        .err()
        .expect("missing provider must fail");
        assert!(missing.to_string().contains("not found"));

        insert_provider(
            repo.as_ref(),
            "empty-base-provider",
            "openai",
            " ",
            "secret-a",
            None,
            false,
        )
        .await;
        let empty_base = resolve_provider(
            &selected_model("empty-base-provider", "model-a", None),
            repo.as_ref(),
            &TEST_KEY,
        )
        .await
        .err()
        .expect("empty base URL must fail");
        assert!(empty_base.to_string().contains("empty base URL"));

        insert_provider(
            repo.as_ref(),
            "decrypt-provider",
            "openai",
            "https://provider.internal/v1",
            "secret-b",
            None,
            false,
        )
        .await;
        let wrong_key = [0x24; 32];
        let decrypt = resolve_provider(
            &selected_model("decrypt-provider", "model-a", None),
            repo.as_ref(),
            &wrong_key,
        )
        .await
        .err()
        .expect("invalid encryption key must fail");
        assert!(decrypt.to_string().contains("Failed to decrypt API key"));
    }

    #[test]
    fn session_home_is_stable_and_isolated() {
        let root = Path::new("data");
        let first = session_home(root, "conversation-a");
        let expected_parent = root.join("hermes-sessions");
        assert_eq!(first, session_home(root, "conversation-a"));
        assert_ne!(first, session_home(root, "conversation-b"));
        assert_eq!(first.parent(), Some(expected_parent.as_path()));
    }

    #[tokio::test]
    async fn minimal_config_contains_no_session_provider_data() {
        let temp = tempfile::tempdir().unwrap();
        let home = session_home(temp.path(), "conversation-secret-check");
        let config_path = ensure_minimal_config(&home).await.unwrap();
        let contents = tokio::fs::read_to_string(config_path).await.unwrap();

        assert_eq!(contents.as_bytes(), HERMES_CONFIG);
        for forbidden in ["sk-secret", "https://provider.internal", "private-model"] {
            assert!(!contents.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn minimal_config_atomically_replaces_stale_contents() {
        let temp = tempfile::tempdir().unwrap();
        let home = session_home(temp.path(), "conversation-replace-check");
        tokio::fs::create_dir_all(&home).await.unwrap();
        let config_path = home.join("config.yaml");
        tokio::fs::write(&config_path, b"stale: true\n").await.unwrap();

        ensure_minimal_config(&home).await.unwrap();

        assert_eq!(tokio::fs::read(&config_path).await.unwrap(), HERMES_CONFIG);
        let mut entries = tokio::fs::read_dir(&home).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "temporary file leaked: {name}");
        }
    }

    #[test]
    fn session_overlay_overrides_forged_metadata_env_and_deduplicates() {
        let mut env = vec![
            env_var("OPENAI_API_KEY", "forged-key"),
            env_var("openai_api_key", "second-forged-key"),
            env_var("HERMES_HOME", "forged-home"),
            env_var("UNRELATED", "preserved"),
        ];
        apply_env_overlay(
            &mut env,
            vec![
                env_var("OPENAI_API_KEY", "session-key"),
                env_var("HERMES_HOME", "session-home"),
            ],
        );

        assert_eq!(env_value(&env, "OPENAI_API_KEY"), Some("session-key"));
        assert_eq!(env_value(&env, "HERMES_HOME"), Some("session-home"));
        assert_eq!(env_value(&env, "UNRELATED"), Some("preserved"));
        assert_eq!(
            env.iter()
                .filter(|entry| entry.name.eq_ignore_ascii_case("OPENAI_API_KEY"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_session_overlays_do_not_share_provider_or_home() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        insert_provider(
            repo.as_ref(),
            "provider-a",
            "openai",
            "https://provider-a.internal/v1",
            "secret-a",
            None,
            false,
        )
        .await;
        insert_provider(
            repo.as_ref(),
            "provider-b",
            "openai",
            "https://provider-b.internal/v1",
            "secret-b",
            None,
            false,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let model_a = selected_model("provider-a", "model-a", None);
        let model_b = selected_model("provider-b", "model-b", None);

        let (overlay_a, overlay_b) = tokio::join!(
            build_session_overlay(&model_a, repo.as_ref(), &TEST_KEY, temp.path(), "conversation-a"),
            build_session_overlay(&model_b, repo.as_ref(), &TEST_KEY, temp.path(), "conversation-b")
        );
        let overlay_a = overlay_a.unwrap();
        let overlay_b = overlay_b.unwrap();

        assert_ne!(
            env_value(&overlay_a.env, "HERMES_HOME"),
            env_value(&overlay_b.env, "HERMES_HOME")
        );
        assert_eq!(env_value(&overlay_a.env, "OPENAI_API_KEY"), Some("secret-a"));
        assert_eq!(env_value(&overlay_b.env, "OPENAI_API_KEY"), Some("secret-b"));
        assert_eq!(env_value(&overlay_a.env, "HERMES_INFERENCE_MODEL"), Some("model-a"));
        assert_eq!(env_value(&overlay_b.env, "HERMES_INFERENCE_MODEL"), Some("model-b"));
        assert!(!overlay_a.env.iter().any(|entry| entry.value == "secret-b"));
        assert!(!overlay_b.env.iter().any(|entry| entry.value == "secret-a"));
    }

    #[tokio::test]
    async fn apply_if_hermes_forces_session_values_after_existing_env() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        insert_provider(
            repo.as_ref(),
            "provider",
            "openai",
            "https://provider.internal/v1",
            "session-key",
            None,
            false,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let mut command_spec = CommandSpec {
            env: vec![
                env_var("OPENAI_BASE_URL", "https://forged.invalid"),
                env_var("OPENAI_API_KEY", "forged-key"),
                env_var("HERMES_INFERENCE_MODEL", "forged-model"),
            ],
            ..CommandSpec::default()
        };

        apply_if_hermes(
            &mut command_spec,
            HermesLaunchPolicy::Managed,
            &selected_model("provider", "model-a", Some("model-b")),
            repo.as_ref(),
            &TEST_KEY,
            temp.path(),
            "conversation",
        )
        .await
        .unwrap();

        assert_eq!(
            env_value(&command_spec.env, "OPENAI_BASE_URL"),
            Some("https://provider.internal/v1")
        );
        assert_eq!(env_value(&command_spec.env, "OPENAI_API_KEY"), Some("session-key"));
        assert_eq!(env_value(&command_spec.env, "HERMES_INFERENCE_MODEL"), Some("model-b"));
        assert_eq!(env_value(&command_spec.env, "HERMES_DISABLE_LAZY_INSTALLS"), Some("1"));
        assert_eq!(
            env_value(&command_spec.env, "HERMES_ACP_SKIP_CONFIGURED_MCP"),
            Some("1")
        );
        assert_eq!(
            env_value(&command_spec.env, "HERMES_ACP_TOOLSET"),
            Some("hermes-acp-lite")
        );
        assert_eq!(
            env_value(&command_spec.env, MANAGED_HERMES_ENV_ISOLATION_MARKER),
            Some("1")
        );
    }

    #[tokio::test]
    async fn non_managed_builtin_hermes_remains_user_managed() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let mut command_spec = CommandSpec::default();

        apply_if_hermes(
            &mut command_spec,
            HermesLaunchPolicy::External,
            &selected_model("missing-provider", "model-a", None),
            repo.as_ref(),
            &TEST_KEY,
            temp.path(),
            "conversation",
        )
        .await
        .unwrap();

        assert!(command_spec.env.is_empty());
        assert_eq!(env_value(&command_spec.env, MANAGED_HERMES_ENV_ISOLATION_MARKER), None);
        assert!(!temp.path().join("hermes-sessions").exists());
    }

    #[tokio::test]
    async fn apply_if_hermes_is_noop_for_other_backends() {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        let temp = tempfile::tempdir().unwrap();
        let original_env = vec![env_var("OPENAI_API_KEY", "unchanged")];
        let mut command_spec = CommandSpec {
            env: original_env.clone(),
            ..CommandSpec::default()
        };

        apply_if_hermes(
            &mut command_spec,
            HermesLaunchPolicy::NotHermes,
            &selected_model("missing-provider", "", None),
            repo.as_ref(),
            &TEST_KEY,
            temp.path(),
            "conversation",
        )
        .await
        .unwrap();

        assert_eq!(command_spec.env, original_env);
        assert!(!temp.path().join("hermes-sessions").exists());
    }

    fn env_value<'a>(env: &'a [EnvVar], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .map(|entry| entry.value.as_str())
    }
}
