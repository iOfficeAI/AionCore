//! Integration tests for agent type implementations and auxiliary features.
//!
//! These tests validate:
//! - Each agent manager implements IAgentTask correctly
//! - Agent factory can build all agent types
//! - Idle scanner finds eligible tasks
//! - Workspace browsing works with real filesystem
//! - Aionrs stub returns appropriate errors

use std::sync::Arc;

use aionui_ai_agent::manager::aionrs::AionrsAgentManager;
use aionui_ai_agent::task_manager::AgentFactory;
use aionui_ai_agent::types::{AionrsResolvedConfig, BuildTaskOptions, SendMessageData};
use aionui_ai_agent::*;
use aionui_ai_agent::{SkillIndex, build_system_instructions_with_skills_index};
use aionui_common::{AgentKillReason, AgentType, ConversationStatus, ProviderWithModel, TimestampMs, now_ms};
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Mock agent for WorkerTaskManager tests with different agent types
// ---------------------------------------------------------------------------

struct TypedMockAgent {
    agent_type: AgentType,
    conversation_id: String,
    workspace: String,
    status: Option<ConversationStatus>,
    last_activity: AtomicI64,
    event_tx: broadcast::Sender<AgentStreamEvent>,
}

impl TypedMockAgent {
    fn new(agent_type: AgentType, conversation_id: &str, status: Option<ConversationStatus>) -> Self {
        let (event_tx, _) = broadcast::channel(16);
        Self {
            agent_type,
            conversation_id: conversation_id.to_owned(),
            workspace: "/tmp/test".to_owned(),
            status,
            last_activity: AtomicI64::new(now_ms()),
            event_tx,
        }
    }

    fn with_last_activity(mut self, ts: TimestampMs) -> Self {
        self.last_activity = AtomicI64::new(ts);
        self
    }
}

#[async_trait::async_trait]
impl IAgentTask for TypedMockAgent {
    fn agent_type(&self) -> AgentType {
        self.agent_type
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        &self.workspace
    }
    fn status(&self) -> Option<ConversationStatus> {
        self.status
    }
    fn last_activity_at(&self) -> TimestampMs {
        self.last_activity.load(Ordering::Relaxed)
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, _data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
        Ok(())
    }
    async fn cancel(&self) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
}

impl IMockAgent for TypedMockAgent {}

// ---------------------------------------------------------------------------
// Aionrs agent tests (real implementation with AgentEngine)
// ---------------------------------------------------------------------------

fn make_aionrs_config() -> AionrsResolvedConfig {
    AionrsResolvedConfig {
        provider: "anthropic".into(),
        api_key: "sk-test-key".into(),
        model: "claude-sonnet-4-20250514".into(),
        base_url: None,
        system_prompt: None,
        max_tokens: 4096,
        max_turns: None,
        compat_overrides: Default::default(),
        session_directory: std::env::temp_dir().join("aionrs-test-sessions"),
        session_mode: None,
        extra_mcp_servers: Default::default(),
        bedrock_config: None,
    }
}

#[tokio::test]
async fn aionrs_agent_kill_succeeds() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/proj".into(), make_aionrs_config(), None)
        .await
        .unwrap();
    assert!(agent.kill(None).is_ok());
    assert!(agent.kill(Some(AgentKillReason::IdleTimeout)).is_ok());
}

#[tokio::test]
async fn aionrs_agent_confirm_succeeds() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/proj".into(), make_aionrs_config(), None)
        .await
        .unwrap();
    // `confirm` is an inherent method on `AionrsAgentManager` (reached via
    // `AgentInstance::Aionrs(..)` in production); the test calls it
    // directly on the concrete manager.
    let result = agent.confirm("msg", "call", json!({}), false);
    assert!(result.is_ok());
}

#[tokio::test]
async fn aionrs_agent_metadata() {
    let agent = AionrsAgentManager::new("conv-abc".into(), "/work".into(), make_aionrs_config(), None)
        .await
        .unwrap();
    assert_eq!(agent.agent_type(), AgentType::Aionrs);
    assert_eq!(agent.workspace(), "/work");
    assert_eq!(agent.conversation_id(), "conv-abc");
    assert_eq!(agent.status(), Some(ConversationStatus::Pending));
    assert!(agent.get_confirmations().is_empty());
    assert!(!agent.check_approval("any", None));
}

// ---------------------------------------------------------------------------
// Idle scanner: collect_idle only finds ACP tasks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn collect_idle_ignores_non_acp_agent_types() {
    use futures_util::FutureExt;
    let old_ts = now_ms() - 600_000; // 10 min ago

    // Build a factory that creates typed mocks (all finished + old)
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        async move {
            let agent_type = opts.context.conversation.agent_type;
            let conversation_id = opts.context.conversation.conversation_id.clone();
            let mock = TypedMockAgent::new(agent_type, &conversation_id, Some(ConversationStatus::Finished))
                .with_last_activity(old_ts);
            Ok(AgentInstance::Mock(Arc::new(mock)))
        }
        .boxed()
    });
    let mgr = WorkerTaskManagerImpl::new(factory);

    let make_opts = |agent_type: AgentType, id: &str| {
        let kind = match agent_type {
            AgentType::Acp => AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
                config: Default::default(),
                belongs_to_team: false,
                session_id: None,
                session_snapshot: None,
            })),
            AgentType::Aionrs => AgentSessionKind::Aionrs(Box::new(AionrsSessionBuildContext {
                config: Default::default(),
                belongs_to_team: false,
            })),
            AgentType::OpenclawGateway => AgentSessionKind::OpenClaw(Box::new(OpenClawSessionBuildContext {
                config: aionui_api_types::OpenClawBuildExtra {
                    backend: None,
                    agent_name: None,
                    gateway: Default::default(),
                    skills: vec![],
                    preset_assistant_id: None,
                    cron_job_id: None,
                    session_key: None,
                },
            })),
            AgentType::Remote => AgentSessionKind::Remote(RemoteSessionBuildContext {
                remote_agent_id: "remote-1".into(),
            }),
            AgentType::Nanobot => AgentSessionKind::Nanobot(NanobotSessionBuildContext),
            AgentType::Gemini => AgentSessionKind::Gemini,
        };
        BuildTaskOptions::new(AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: id.into(),
                user_id: "user-1".into(),
                agent_type,
                source: None,
            },
            workspace: WorkspaceContext {
                path: "/tmp".into(),
                stored_path: "/tmp".into(),
                is_custom: true,
            },
            model: ProviderWithModel {
                provider_id: "p".into(),
                model: "m".into(),
                use_model: None,
            },
            skills: vec![],
            kind,
        })
    };

    mgr.get_or_build_task("nanobot-1", make_opts(AgentType::Nanobot, "nanobot-1"))
        .await
        .unwrap();
    mgr.get_or_build_task("openclaw-1", make_opts(AgentType::OpenclawGateway, "openclaw-1"))
        .await
        .unwrap();
    mgr.get_or_build_task("acp-1", make_opts(AgentType::Acp, "acp-1"))
        .await
        .unwrap();
    mgr.get_or_build_task("remote-1", make_opts(AgentType::Remote, "remote-1"))
        .await
        .unwrap();

    assert_eq!(mgr.active_count(), 4);

    // Only ACP should be collected
    let idle = mgr.collect_idle(300_000); // 5-min threshold
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0], "acp-1");
}

// ---------------------------------------------------------------------------
// Workspace browsing (uses real filesystem via tempdir)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn workspace_browse_reads_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let base = tmp.path();

    // Create test files and dirs
    std::fs::create_dir(base.join("src")).unwrap();
    std::fs::create_dir(base.join("tests")).unwrap();
    std::fs::write(base.join("Cargo.toml"), "# test").unwrap();
    std::fs::write(base.join("README.md"), "# readme").unwrap();

    let mut entries = Vec::new();
    let mut dir_reader = tokio::fs::read_dir(base).await.unwrap();
    while let Ok(Some(entry)) = dir_reader.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ft = entry.file_type().await.unwrap();
        let entry_type = if ft.is_dir() { "directory" } else { "file" };
        entries.push((name, entry_type.to_string()));
    }

    assert_eq!(entries.len(), 4);

    // Check that directories exist
    let dir_names: Vec<&str> = entries
        .iter()
        .filter(|(_, t)| t == "directory")
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(dir_names.contains(&"src"));
    assert!(dir_names.contains(&"tests"));

    // Check that files exist
    let file_names: Vec<&str> = entries
        .iter()
        .filter(|(_, t)| t == "file")
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(file_names.contains(&"Cargo.toml"));
    assert!(file_names.contains(&"README.md"));
}

// ---------------------------------------------------------------------------
// build_system_instructions_with_skills_index (M-16 fix)
// ---------------------------------------------------------------------------

#[test]
fn build_system_instructions_with_skills_index_empty() {
    let result = build_system_instructions_with_skills_index("Base prompt", &[]);
    assert_eq!(result, "Base prompt");
}

#[test]
fn build_system_instructions_with_skills_index_appends_index() {
    let skills = vec![
        SkillIndex {
            name: "review".into(),
            description: "Code review".into(),
        },
        SkillIndex {
            name: "debug".into(),
            description: "Debugging".into(),
        },
    ];
    let result = build_system_instructions_with_skills_index("You are an AI assistant.", &skills);
    assert!(result.starts_with("You are an AI assistant."));
    assert!(result.contains("## Available Skills"));
    assert!(result.contains("- **review**: Code review"));
    assert!(result.contains("- **debug**: Debugging"));
    assert!(result.contains("[LOAD_SKILL: skill-name]"));
}

// ---------------------------------------------------------------------------
// Agent type metadata validation
// ---------------------------------------------------------------------------

#[test]
fn agent_type_serde_all_variants() {
    // Verify that all AgentType variants serialize/deserialize correctly
    for (variant, expected_json) in [
        (AgentType::Acp, "\"acp\""),
        (AgentType::OpenclawGateway, "\"openclaw-gateway\""),
        (AgentType::Nanobot, "\"nanobot\""),
        (AgentType::Remote, "\"remote\""),
        (AgentType::Aionrs, "\"aionrs\""),
    ] {
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, expected_json, "Failed for {:?}", variant);
        let parsed: AgentType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }
}
