//! Opt-in real-agent validation for #946, complementing the deterministic
//! regressions in aionui-conversation/tests/tool_output_preview.rs.
//!
//! Needs a logged-in Codex CLI and python3; spends model tokens. Credentials are
//! copied into a disposable CLI home, never logged. User config, skills, MCP
//! settings and project data are not copied. No production service is contacted.
//! Only aggregate evidence prints; this is not a sandbox/privacy audit of Codex.
//!
//! Run with AIONUI_LIVE_CODEX_BIN and AIONUI_LIVE_CODEX_AUTH_FILE explicitly set:
//! cargo test -p aionui-ai-agent --test live_tool_output_preview -- --ignored --nocapture
//! Optional AIONUI_LIVE_CODEX_MODEL selects a model via the CLI's documented -c flag.
//! Uses the production RealSpawner -> CodexConnection -> SessionAgentTask path;
//! ObservedBackend below only observes, never synthesizes or delays input events.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_ai_agent::{
    AgentStreamEvent, IAgentTask,
    protocol::events::{ToolCallEventData, ToolCallStatus},
    session_agent::SessionAgentTask,
};
use aionui_common::{AgentType, EnvVar};
use aionui_session::{
    BackendConnection, BackendError, Capabilities, CodexConnection, Command, CommandMeta, CommandReceipt, ContentBlock,
    SessionBackend, SessionConfig, SessionEnvelope, SessionEvent, SessionSpec, ToolResultContent,
};
use futures_util::{StreamExt, stream::BoxStream};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

const HALF: usize = 512 * 1024;
const MIB: usize = 1024 * 1024;
const GENERATOR: &str = r#"import pathlib, sys, time
chunk = ('é🙂' + 'x' * 8186).encode('utf-8')
assert len(chunk) == 8192
for phase in range(2):
    for _ in range(64):
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        time.sleep(0.04)
    if phase == 0:
        deadline = time.monotonic() + 30
        while not pathlib.Path('continue').exists():
            if time.monotonic() > deadline:
                raise RuntimeError('test reader did not release the checkpoint')
            time.sleep(0.02)
"#;

#[derive(Default)]
struct Observed {
    delta_bytes: usize,
    deltas: usize,
    finals: HashMap<String, (usize, Vec<u8>, bool)>,
    terminal: Option<bool>,
    notices: usize,
}

struct ObservedBackend {
    real: Arc<dyn SessionBackend>,
    observed: Arc<Mutex<Observed>>,
}

#[async_trait::async_trait]
impl SessionBackend for ObservedBackend {
    async fn dispatch(&self, command: Command) -> Result<CommandReceipt, BackendError> {
        self.real.dispatch(command).await
    }

    fn capabilities(&self) -> Capabilities {
        self.real.capabilities()
    }

    fn events(&self) -> BoxStream<'static, SessionEnvelope> {
        let observed = self.observed.clone();
        self.real
            .events()
            .inspect(move |envelope| {
                let mut seen = observed.lock().unwrap();
                match &envelope.event {
                    SessionEvent::ToolOutputDelta { text, .. } => {
                        seen.delta_bytes += text.len();
                        seen.deltas += 1;
                    }
                    SessionEvent::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        let texts: Vec<_> = content
                            .iter()
                            .filter_map(|c| match c {
                                ToolResultContent::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect();
                        // Match the existing translator's text-block join, not
                        // the original stdout (the CLI may truncate its final).
                        let text = texts.join("\n");
                        seen.finals.insert(
                            tool_use_id.clone(),
                            (text.len(), Sha256::digest(text.as_bytes()).to_vec(), *is_error),
                        );
                    }
                    SessionEvent::TurnResult { is_error, .. } => seen.terminal = Some(*is_error),
                    SessionEvent::Notice { .. } => seen.notices += 1,
                    _ => {}
                }
            })
            .boxed()
    }
}

fn take_pending(rx: &mut broadcast::Receiver<AgentStreamEvent>) -> Vec<AgentStreamEvent> {
    let mut events = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => events.push(event),
            Err(broadcast::error::TryRecvError::Empty) => return events,
            Err(error) => panic!("live subscriber lost events: {error}"),
        }
    }
}

fn inspect_frame(data: &ToolCallEventData, terminal_ids: &mut HashSet<String>) -> usize {
    if data.status == ToolCallStatus::Running {
        assert!(!terminal_ids.contains(&data.call_id), "preview after final");
        let bytes = data.output.as_deref().map_or(0, str::len);
        assert!(bytes <= 64 * 1024, "oversized preview: {bytes}");
        bytes
    } else {
        terminal_ids.insert(data.call_id.clone());
        0
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "LIVE: real Codex login required; executes synthetic output and spends tokens"]
async fn real_codex_with_stalled_then_slow_reader() {
    let binary = std::env::var_os("AIONUI_LIVE_CODEX_BIN").expect("set AIONUI_LIVE_CODEX_BIN");
    let auth = std::env::var_os("AIONUI_LIVE_CODEX_AUTH_FILE").expect("set AIONUI_LIVE_CODEX_AUTH_FILE");
    let mut version_probe = aionui_runtime::Builder::clean_cli(&binary);
    version_probe.arg("--version");
    let version = version_probe.output().await.expect("read Codex version");
    assert!(version.status.success());
    eprintln!("live CLI: {}", String::from_utf8_lossy(&version.stdout).trim());
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    let cli_home = tmp.path().join("codex-home");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&cli_home).unwrap();
    std::fs::copy(auth, cli_home.join("auth.json")).unwrap();
    std::fs::write(workspace.join("emit.py"), GENERATOR).unwrap();
    // Only this disposable synthetic workspace is trusted. This never edits
    // the user's global config or weakens the workspace-write sandbox.
    std::fs::write(
        cli_home.join("config.toml"),
        format!(
            "[projects.{}]\ntrust_level = \"trusted\"\n",
            serde_json::to_string(&workspace.to_string_lossy()).unwrap()
        ),
    )
    .unwrap();
    let mut extra_args = Vec::new();
    if let Ok(model) = std::env::var("AIONUI_LIVE_CODEX_MODEL") {
        extra_args.extend(["-c".into(), format!("model={}", serde_json::to_string(&model).unwrap())]);
    }
    let spawner = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(tmp.path())),
        uuid::Uuid::now_v7(),
        "live-preview-test",
    ));
    let real = tokio::time::timeout(
        Duration::from_secs(30),
        CodexConnection::new(spawner).open_session(
            SessionSpec::Fresh {
                session_id: "live-preview".into(),
            },
            SessionConfig {
                cwd: Some(workspace.to_string_lossy().into_owned()),
                cli_program: Some(binary.into()),
                approval_policy: Some("never".into()),
                spawn_env: vec![EnvVar {
                    name: "CODEX_HOME".into(),
                    value: cli_home.to_string_lossy().into_owned(),
                }],
                extra_args,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("real Codex handshake timeout")
    .expect("real Codex session opens");
    let observed = Arc::new(Mutex::new(Observed::default()));
    let task = SessionAgentTask::new(
        AgentType::Acp,
        "live-preview".into(),
        "test-user".into(),
        workspace.to_string_lossy().into_owned(),
        Arc::new(ObservedBackend {
            real: real.clone(),
            observed: observed.clone(),
        }),
        None,
    );
    let mut rx = task.subscribe();
    tokio::time::timeout(
        Duration::from_secs(30),
        real.dispatch(Command::Send {
            content: vec![ContentBlock::Text(
                "Run exactly python3 emit.py in the current directory. This is an output-streaming test: \
             do not read or modify any files, do not redirect, filter or capture stdout. \
             The test harness releases a checkpoint file after reading the first half of stdout. \
             Allow up to 60 seconds, wait for the command to finish, then reply only DONE."
                    .into(),
            )],
            metadata: CommandMeta::default(),
        }),
    )
    .await
    .expect("real dispatch timeout")
    .expect("real prompt accepted");

    // The producer pauses after 512 KiB until we release its checkpoint. The
    // production backend/pump keep draining; only the outer subscriber stalls.
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            {
                let seen = observed.lock().unwrap();
                assert!(seen.terminal.is_none(), "turn ended before the streaming checkpoint");
                if seen.delta_bytes >= HALF {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let seen = observed.lock().unwrap();
        panic!(
            "no checkpoint: delta_bytes={}, deltas={}, notices={}",
            seen.delta_bytes, seen.deltas, seen.notices
        )
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let pending = take_pending(&mut rx);
    let preview_bytes: usize = pending
        .iter()
        .filter_map(|event| match event {
            AgentStreamEvent::ToolCall(data) if data.status == ToolCallStatus::Running => {
                Some(data.output.as_deref().map_or(0, str::len))
            }
            _ => None,
        })
        .sum();
    eprintln!(
        "live checkpoint: retained_preview_bytes={preview_bytes}, queued_events={}",
        pending.len()
    );
    assert!(preview_bytes > 0, "must exercise actual output preview delivery");
    assert!(preview_bytes <= MIB, "unbounded live preview copies: {preview_bytes}");
    let mut terminal_ids = HashSet::new();
    for event in pending {
        if let AgentStreamEvent::ToolCall(data) = event {
            inspect_frame(&data, &mut terminal_ids);
        }
    }
    std::fs::write(workspace.join("continue"), b"release").unwrap();
    let mut received_finals = HashMap::new();
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let event = rx.recv().await.expect("slow reader must not lag");
            if let AgentStreamEvent::ToolCall(data) = &event {
                inspect_frame(data, &mut terminal_ids);
                if data.status != ToolCallStatus::Running {
                    assert_eq!(
                        data.status,
                        ToolCallStatus::Completed,
                        "synthetic command must complete"
                    );
                    let text = data.output.as_deref().unwrap_or_default();
                    received_finals.insert(
                        data.call_id.clone(),
                        (text.len(), Sha256::digest(text.as_bytes()).to_vec()),
                    );
                }
            }
            if matches!(event, AgentStreamEvent::Finish(_)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("real turn finishes with slow subscriber");
    tokio::time::sleep(Duration::from_millis(300)).await;
    for event in take_pending(&mut rx) {
        if let AgentStreamEvent::ToolCall(data) = event {
            inspect_frame(&data, &mut terminal_ids);
        }
    }
    let seen = observed.lock().unwrap();
    assert!(seen.delta_bytes >= MIB, "real CLI must stream the full synthetic MiB");
    assert!(seen.deltas >= 16, "exercise multiple real CLI deltas");
    assert_eq!(seen.terminal, Some(false));
    assert!(!seen.finals.is_empty());
    for (id, (len, hash, error)) in &seen.finals {
        assert!(!error, "synthetic command failed");
        assert_eq!(
            received_finals.get(id),
            Some(&(*len, hash.clone())),
            "final changed in pump"
        );
    }
    eprintln!(
        "live final: delta_bytes={}, deltas={}, final_bytes={}, finals={}, terminal_error=false; no late preview",
        seen.delta_bytes,
        seen.deltas,
        seen.finals.values().map(|(len, _, _)| len).sum::<usize>(),
        seen.finals.len(),
    );
}
