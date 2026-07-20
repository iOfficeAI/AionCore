use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::time::Duration;

use aion_config::config::{McpServerConfig, TransportType};
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::timeout;

use super::*;
use crate::agent_task::IAgentTask;
use crate::protocol::events::FinishEventData;

async fn assert_no_stop_signal(agent: &AionrsAgentManager) {
    let notified = agent.cancel_notify.notified();
    tokio::pin!(notified);

    assert!(
        timeout(Duration::from_millis(20), &mut notified).await.is_err(),
        "idle stop must not leave a stale cancellation signal for the next turn"
    );
}

fn make_test_config() -> AionrsResolvedConfig {
    AionrsResolvedConfig {
        provider: "anthropic".into(),
        api_key: "sk-test-key".into(),
        model: "claude-sonnet-4-20250514".into(),
        base_url: None,
        system_prompt: None,
        max_tokens: Some(4096),
        max_turns: None,
        max_tool_call_malformed_turns: None,
        max_tool_call_failure_turns: None,
        compat_overrides: Default::default(),
        session_directory: env::temp_dir().join("aionrs-test-sessions"),
        session_mode: None,
        skills: Vec::new(),
        extra_mcp_servers: HashMap::new(),
        bedrock_config: None,
        runtime_env: Vec::new(),
        prompt_dump_dir: None,
    }
}

#[test]
fn aionrs_final_input_dump_value_contains_raw_split_input_and_context() {
    let mut mcp_env = HashMap::new();
    mcp_env.insert("TOKEN".to_owned(), "raw-token-value".to_owned());

    let mut mcp_servers = HashMap::new();
    mcp_servers.insert(
        "raw-mcp".to_owned(),
        McpServerConfig {
            transport: TransportType::Stdio,
            command: Some("/bin/raw-mcp".to_owned()),
            args: Some(vec!["--serve".to_owned()]),
            env: Some(mcp_env),
            url: None,
            headers: None,
            deferred: Some(false),
            startup_timeout_ms: None,
        },
    );

    let context = AionrsFinalInputDumpContext {
        dump_dir: PathBuf::from("/tmp/prompt-dumps"),
        provider: "openai".to_owned(),
        model: "gpt-test".to_owned(),
        base_url: Some("https://example.test/v1".to_owned()),
        system_prompt: Some("assistant rule raw".to_owned()),
        session_mode: Some("yolo".to_owned()),
        skills: vec!["aionui-config".to_owned()],
        mcp_servers,
        runtime_env: vec![("AIONUI_RAW".to_owned(), "raw-env-value".to_owned())],
    };
    let data = SendMessageData {
        content: "team wake raw content".to_owned(),
        msg_id: "msg-aionrs-final".to_owned(),
        turn_id: Some("turn-aionrs-final".to_owned()),
        files: Vec::new(),
        inject_skills: Vec::new(),
    };

    let value = build_aionrs_final_input_dump_value("conv-aionrs", "/workspace", &context, &data);

    assert_eq!(value["kind"], "aionrs-final-input");
    assert_eq!(value["backend"], "aionrs");
    assert_eq!(value["conversation_id"], "conv-aionrs");
    assert_eq!(value["msg_id"], "msg-aionrs-final");
    assert_eq!(value["turn_id"], "turn-aionrs-final");
    assert_eq!(value["input"]["system_prompt"], "assistant rule raw");
    assert_eq!(value["input"]["user_content"], "team wake raw content");
    assert_eq!(value["resolved_context"]["provider"], "openai");
    assert_eq!(value["resolved_context"]["model"], "gpt-test");
    assert_eq!(value["resolved_context"]["workspace"]["path"], "/workspace");
    assert_eq!(value["resolved_context"]["skills"][0], "aionui-config");
    assert_eq!(
        value["resolved_context"]["mcp_servers"]["raw-mcp"]["env"]["TOKEN"],
        "raw-token-value"
    );
    assert_eq!(value["resolved_context"]["runtime_env"][0][1], "raw-env-value");
}

#[tokio::test]
async fn aionrs_agent_returns_correct_type() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert_eq!(agent.agent_type(), AgentType::Aionrs);
    assert_eq!(agent.workspace(), "/project");
    assert_eq!(agent.conversation_id(), "conv-1");
}

#[tokio::test]
async fn aionrs_agent_initial_status_is_pending() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert_eq!(agent.status(), Some(ConversationStatus::Pending));
}

#[tokio::test]
async fn aionrs_agent_subscribe_returns_receiver() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    let _rx = agent.subscribe();
}

#[tokio::test]
async fn aionrs_agent_kill_succeeds() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert!(agent.kill(None).is_ok());
    // Idle kill only clears transient state; task-manager removal owns lifecycle cleanup.
    assert_eq!(agent.status(), Some(ConversationStatus::Pending));
}

#[tokio::test]
async fn aionrs_agent_kill_with_reason_succeeds() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert!(agent.kill(Some(AgentKillReason::IdleTimeout)).is_ok());
}

#[tokio::test]
async fn aionrs_agent_kill_running_turn_sends_stop_signal() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    agent.runtime.reset_for_new_turn(ConversationStatus::Running);

    let notified = agent.cancel_notify.notified();
    tokio::pin!(notified);
    assert!(timeout(Duration::from_millis(20), &mut notified).await.is_err());

    agent
        .kill(Some(AgentKillReason::ConversationDeleted))
        .expect("kill should request stop");

    timeout(Duration::from_millis(50), &mut notified)
        .await
        .expect("running kill should wake in-flight turn");
}

#[tokio::test]
async fn aionrs_agent_kill_and_wait_waits_for_running_turn_terminal() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    agent.runtime.reset_for_new_turn(ConversationStatus::Running);

    let wait = agent.kill_and_wait(Some(AgentKillReason::ConversationDeleted));
    tokio::pin!(wait);
    assert!(
        timeout(Duration::from_millis(20), &mut wait).await.is_err(),
        "kill_and_wait must not return before a running turn reaches a terminal event"
    );

    agent.runtime.emit_finish(None);
    agent.turn_finished_notify.notify_waiters();

    timeout(Duration::from_millis(50), &mut wait)
        .await
        .expect("kill_and_wait should return after terminal notification");
}

#[tokio::test]
async fn aionrs_agent_kill_idle_turn_does_not_leave_stale_stop_signal() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();

    agent
        .kill(Some(AgentKillReason::ConversationDeleted))
        .expect("idle kill should be harmless");

    assert_no_stop_signal(&agent).await;
}

#[tokio::test]
async fn aionrs_agent_confirmations_initially_empty() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert!(agent.get_confirmations().is_empty());
}

#[tokio::test]
async fn aionrs_agent_get_slash_commands_does_not_wait_for_engine_lock() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();

    let _engine_guard = agent.engine.lock().await;
    let commands = timeout(Duration::from_millis(50), agent.get_slash_commands())
        .await
        .expect("slash command metadata should not wait for an active engine run")
        .unwrap();

    assert!(!commands.is_empty());
}

#[tokio::test]
async fn aionrs_agent_check_approval_returns_false_by_default() {
    let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    assert!(!agent.check_approval("any_action", None));
}

#[tokio::test]
async fn stop_only_signals_in_flight_run() {
    let agent = AionrsAgentManager::new("conv-stop".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    let mut rx = agent.subscribe();

    agent.cancel().await.unwrap();

    assert_eq!(agent.status(), Some(ConversationStatus::Pending));
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    assert_no_stop_signal(&agent).await;
}

#[tokio::test]
async fn runtime_can_emit_error_and_finish() {
    let agent = AionrsAgentManager::new("conv-err".into(), "/project".into(), make_test_config(), None)
        .await
        .unwrap();
    let mut rx = agent.subscribe();

    agent.runtime.emit_error("test error");
    // emit_error sets status to Finished, so emit_finish is a no-op here.
    // We emit directly for the Finish broadcast path test:
    agent
        .runtime
        .emit(AgentStreamEvent::Finish(FinishEventData { session_id: None }));

    match rx.try_recv().unwrap() {
        AgentStreamEvent::Error(data) => assert_eq!(data.message, "test error"),
        other => panic!("Expected Error, got {:?}", other),
    }
    match rx.try_recv().unwrap() {
        AgentStreamEvent::Finish(_) => {}
        other => panic!("Expected Finish, got {:?}", other),
    }
}
