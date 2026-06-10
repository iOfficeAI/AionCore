use std::path::PathBuf;
use std::sync::Arc;

use aionui_ai_agent::AcpSessionSyncService;
use aionui_ai_agent::AcpSkillManager;
use aionui_ai_agent::factory::{AgentFactoryDeps, build_agent_factory};
use aionui_ai_agent::registry::AgentRegistry;
use aionui_ai_agent::session_context::{
    AgentSessionContext, AgentSessionKind, AionrsSessionBuildContext, ConversationContext, WorkspaceContext,
};
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_api_types::AionrsBuildExtra;
use aionui_common::{AgentType, ProviderWithModel, encrypt_string};
use aionui_db::{
    CreateProviderParams, IAcpSessionRepository, IProviderRepository, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteProviderRepository, init_database_memory,
};
use aionui_realtime::BroadcastEventBus;

fn test_encryption_key() -> [u8; 32] {
    [0xABu8; 32]
}

async fn setup() -> (
    Arc<dyn IProviderRepository>,
    Arc<AgentRegistry>,
    Arc<AcpSessionSyncService>,
) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(pool.clone()));
    let metadata_repo = Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let registry = AgentRegistry::new(metadata_repo);
    registry.hydrate().await.unwrap();
    let session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let acp_agent_service = AcpSessionSyncService::new(session_repo);
    (provider_repo, registry, acp_agent_service)
}

async fn insert_test_provider(repo: &dyn IProviderRepository, id: &str, platform: &str) {
    let key = test_encryption_key();
    let encrypted_api_key = encrypt_string("sk-test-key-12345", &key).unwrap();
    repo.create(CreateProviderParams {
        id: Some(id),
        platform,
        name: "Test Provider",
        base_url: "https://api.example.com/v1",
        api_key_encrypted: &encrypted_api_key,
        models: r#"["gpt-4o","gpt-5.4"]"#,
        enabled: true,
        capabilities: "[]",
        context_limit: None,
        model_protocols: None,
        model_enabled: None,
        model_health: None,
        bedrock_config: None,
        is_full_url: false,
    })
    .await
    .unwrap();
}

fn make_factory(
    provider_repo: Arc<dyn IProviderRepository>,
    agent_registry: Arc<AgentRegistry>,
    acp_agent_service: Arc<AcpSessionSyncService>,
) -> aionui_ai_agent::task_manager::AgentFactory {
    let tmp = tempfile::TempDir::new().unwrap();
    let skill_paths = Arc::new(aionui_extension::resolve_skill_paths(tmp.path(), tmp.path()));
    build_agent_factory(AgentFactoryDeps {
        skill_manager: AcpSkillManager::new(skill_paths),
        provider_repo,
        encryption_key: test_encryption_key(),
        agent_registry,
        acp_agent_service,
        data_dir: PathBuf::from("/tmp/aionrs-test"),
        broadcaster: Arc::new(BroadcastEventBus::new(16)),
        backend_binary_path: Arc::new(PathBuf::from("/tmp/aionrs-test/aioncore")),
        guide_mcp_config: None,
        mcp_server_repo: None,
    })
}

fn make_aionrs_options(
    conversation_id: &str,
    workspace: &str,
    model: ProviderWithModel,
    config: AionrsBuildExtra,
) -> BuildTaskOptions {
    BuildTaskOptions::new(AgentSessionContext {
        conversation: ConversationContext {
            conversation_id: conversation_id.to_owned(),
            user_id: "user-1".to_owned(),
            agent_type: AgentType::Aionrs,
            source: None,
        },
        workspace: WorkspaceContext {
            path: workspace.to_owned(),
            stored_path: workspace.to_owned(),
            is_custom: !workspace.is_empty(),
        },
        model,
        skills: vec![],
        team: None,
        kind: AgentSessionKind::Aionrs(Box::new(AionrsSessionBuildContext {
            config,
            team: None,
            belongs_to_team: false,
        })),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aionrs_factory_returns_error_for_missing_provider() {
    let (provider_repo, agent_registry, acp_agent_service) = setup().await;
    let factory = make_factory(provider_repo, agent_registry, acp_agent_service);

    let options = make_aionrs_options(
        "conv-test-1",
        "",
        ProviderWithModel {
            provider_id: "nonexistent-provider".into(),
            model: "gpt-4o".into(),
            use_model: None,
        },
        AionrsBuildExtra::default(),
    );

    let result = factory(options).await;
    match result {
        Ok(_) => panic!("Expected error for missing provider, got Ok"),
        Err(e) => {
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("not found"),
                "Expected 'not found' error, got: {err_msg}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aionrs_factory_resolves_provider_from_db() {
    let (provider_repo, agent_registry, acp_agent_service) = setup().await;
    insert_test_provider(&*provider_repo, "prov-001", "openai").await;
    let factory = make_factory(provider_repo, agent_registry, acp_agent_service);

    let options = make_aionrs_options(
        "conv-test-2",
        "/tmp/test-workspace",
        ProviderWithModel {
            provider_id: "prov-001".into(),
            model: "gpt-4o".into(),
            use_model: None,
        },
        AionrsBuildExtra {
            max_tokens: 2048,
            ..Default::default()
        },
    );

    let result = factory(options).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aionrs_factory_respects_use_model_override() {
    let (provider_repo, agent_registry, acp_agent_service) = setup().await;
    insert_test_provider(&*provider_repo, "prov-002", "openai").await;
    let factory = make_factory(provider_repo, agent_registry, acp_agent_service);

    let options = make_aionrs_options(
        "conv-test-3",
        "/tmp/test-workspace",
        ProviderWithModel {
            provider_id: "prov-002".into(),
            model: "gpt-4o".into(),
            use_model: Some("gpt-5.4".into()),
        },
        AionrsBuildExtra::default(),
    );

    let result = factory(options).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
}
