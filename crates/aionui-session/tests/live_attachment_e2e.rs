//! LIVE e2e (feature 015 T8, ELECTRON-3R4): probe E productionized.
//!
//! Drives the REAL `ClaudeConnection::open_session` + `dispatch(Send)` path
//! against the real `claude` CLI: a fail-closed default-mode session must read
//! an image staged in the out-of-cwd upload dir (`$TMPDIR/aionui/<conv>`,
//! granted via the spawn's `--add-dir`) with ZERO permission prompts and
//! answer from its content. Before the fix, this exact frame dead-ends on a
//! `can_use_tool(Read)` permission ask.
//!
//! `#[ignore]`: needs a `claude` binary + working credentials on PATH. Run:
//! `cargo test -p aionui-session --test live_attachment_e2e -- --ignored --nocapture`

use std::sync::Arc;

use aionui_session::{
    BackendConnection, ClaudeConnection, Command, CommandMeta, ContentBlock, SessionConfig, SessionEvent, SessionSpec,
};
use futures_util::StreamExt;

/// 32x32 solid-red PNG (generated offline; embedded so the test is hermetic on
/// the input side — only the CLI + credentials are external).
const RED_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 32, 0, 0, 0, 32, 8, 2, 0, 0, 0, 252, 24,
    237, 163, 0, 0, 0, 40, 73, 68, 65, 84, 120, 156, 237, 205, 177, 13, 0, 0, 12, 194, 48, 254, 127, 186, 125, 2, 54,
    75, 153, 227, 92, 50, 109, 123, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 197, 30, 50, 195, 252, 46, 60, 190, 144, 144, 0, 0,
    0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "LIVE: requires claude CLI + credentials on PATH"]
async fn live_t8_default_mode_reads_temp_attachment_without_permission_prompt() {
    // Unique conversation id → unique upload staging dir (cleaned up at the end).
    let conv_id = format!("live-t8-{}", uuid::Uuid::new_v4());
    let upload_dir = aionui_common::paths::uploads_dir(Some(&conv_id));
    std::fs::create_dir_all(&upload_dir).expect("create upload staging dir");
    let image_path = upload_dir.join("image-1.png");
    std::fs::write(&image_path, RED_PNG).expect("stage pasted image");

    // Empty temp workspace — the attachment is OUTSIDE this cwd by construction.
    let workspace = tempfile::tempdir().expect("workspace");
    let registry_dir = tempfile::tempdir().expect("registry");
    let spawner: Arc<dyn aionui_process::Spawner> = Arc::new(aionui_process::RealSpawner::new(
        Arc::new(aionui_process::FileRegistryStore::new(registry_dir.path())),
        uuid::Uuid::new_v4(),
        aionui_process::local_machine_id(registry_dir.path()),
    ));

    // Blank production config: mode None → the spawn's fail-closed
    // `--permission-mode default`. THE point of the test — the read must
    // succeed through the `--add-dir` grant, not through a permissive mode.
    let config = SessionConfig {
        cwd: Some(workspace.path().to_string_lossy().into_owned()),
        ..Default::default()
    };
    let backend = ClaudeConnection::new(spawner)
        .open_session(
            SessionSpec::Fresh {
                session_id: conv_id.clone(),
            },
            config,
        )
        .await
        .expect("open live claude session");

    // Subscribe BEFORE dispatch so no event can be missed.
    let mut events = backend.events();

    // The exact production lowering of a pasted attachment (session_agent
    // send_message): user text + a raw-path ResourceLink, which the claude
    // adapter turns into an `[Attached file: <path>]` text reference.
    let receipt = backend
        .dispatch(Command::Send {
            content: vec![
                ContentBlock::Text(
                    "What is the dominant color of the attached image? Reply with just the color name.".to_string(),
                ),
                ContentBlock::ResourceLink {
                    uri: image_path.to_string_lossy().into_owned(),
                    mime_type: None,
                },
            ],
            metadata: CommandMeta {
                command_id: 1,
                cwd: None,
                extra_args: Vec::new(),
                client_msg_id: None,
            },
        })
        .await
        .expect("dispatch Send");
    assert!(receipt.accepted, "send must be accepted");

    let mut permissions: Vec<String> = Vec::new();
    let mut streamed_text = String::new();
    let mut turn_result: Option<(bool, String)> = None;
    let drained = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        while let Some(env) = events.next().await {
            match env.event {
                SessionEvent::Permission { ref request_id, .. } => permissions.push(request_id.clone()),
                SessionEvent::MessageDelta { ref text, .. } => streamed_text.push_str(text),
                SessionEvent::TurnResult {
                    is_error, result_text, ..
                } => {
                    turn_result = Some((is_error, result_text));
                    return;
                }
                SessionEvent::Detached { .. } => return,
                _ => {}
            }
        }
    })
    .await;

    // Best-effort cleanup before asserting.
    let _ = std::fs::remove_dir_all(&upload_dir);

    assert!(drained.is_ok(), "turn must terminate within 180s (no permission hang)");
    let (is_error, result_text) = turn_result.expect("turn must end in a TurnResult, not a process exit");
    assert!(!is_error, "turn errored: {result_text}");
    assert!(
        permissions.is_empty(),
        "attachment read must not raise permission prompts (ELECTRON-3R4), got: {permissions:?}"
    );
    let answer = format!("{streamed_text} {result_text}").to_ascii_lowercase();
    assert!(
        answer.contains("red"),
        "model must answer from the attachment's content, got: {answer}"
    );
}
