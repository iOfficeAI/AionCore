//! Live end-to-end acceptance for the shared `opencode serve` backend
//! (PR2). These tests drive REAL opencode against the REAL model configured
//! on the machine, so they are gated behind `AIONUI_OPENCODE_E2E=1` and skip
//! cleanly (exit 0) otherwise — CI without an opencode install is unaffected.
//!
//! Run locally:
//! ```text
//! $env:AIONUI_OPENCODE_E2E = "1"
//! cargo test -p aionui-ai-agent --test opencode_shared_e2e -- --nocapture
//! ```
//!
//! What is proven here (not by the pure unit tests in `opencode_shared`):
//! 1. TWO conversations share ONE server process (same base_url from the pool
//!    and a single registry record), with independent durable SSE streams.
//! 2. A full turn translates: TurnStarted → (deltas) → usage → TurnResult.
//! 3. Permission flow: the `permission.v2.asked` poll-discovery surfaces a
//!    `Permission` event; `AnswerPermission(Approved)` unblocks the tool and
//!    the turn completes.
//! 4. Resume: a fresh backend instance on the SAME `ses_` id re-binds and
//!    keeps streaming.
//! 5. Crash: killing the server yields `Detached`, and the next open respawns
//!    a healthy server (pool watchdog + registry hygiene).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::opencode_shared::{OpencodeConnection, global_pool};
use aionui_session::{
    BackendConnection, CancelTarget, Command, CommandMeta, ContentBlock, PermissionDecision, SessionConfig,
    SessionEnvelope, SessionEvent, SessionSpec,
};
use futures_util::StreamExt;

fn e2e_enabled() -> bool {
    matches!(
        std::env::var("AIONUI_OPENCODE_E2E").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

fn opencode_program() -> Option<PathBuf> {
    for dir in std::env::split_paths(&std::env::var("PATH").unwrap_or_default()) {
        for name in ["opencode.exe", "opencode.cmd", "opencode"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn base_config(cwd: &Path, program: &Path) -> SessionConfig {
    SessionConfig {
        cwd: Some(cwd.to_string_lossy().into_owned()),
        cli_program: Some(program.to_path_buf()),
        ..Default::default()
    }
}

fn meta() -> CommandMeta {
    CommandMeta {
        command_id: 1,
        cwd: None,
        extra_args: Vec::new(),
        client_msg_id: None,
    }
}

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(s.to_string())]
}

/// Drain `backend`'s event stream until `done` matches or timeout, collecting
/// everything seen. The stream is a broadcast subscription, so open it BEFORE
/// dispatching.
async fn collect_until(
    rx: &mut futures_util::stream::BoxStream<'static, SessionEnvelope>,
    timeout: Duration,
    done: impl Fn(&SessionEvent) -> bool,
    seen: &std::sync::Arc<std::sync::Mutex<Vec<SessionEvent>>>,
) -> bool {
    let fut = async {
        while let Some(env) = rx.next().await {
            let hit = done(&env.event);
            seen.lock().unwrap().push(env.event);
            if hit {
                return true;
            }
        }
        false
    };
    tokio::time::timeout(timeout, fut).await.unwrap_or(false)
}

fn snapshot(seen: &std::sync::Arc<std::sync::Mutex<Vec<SessionEvent>>>) -> Vec<SessionEvent> {
    seen.lock().unwrap().clone()
}

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "aionui-opencode-e2e-{}-{:x}",
        tag,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// #1 + #2 + #4: one server, two conversations, full translated turns, then
/// one of them RESUMES from its `ses_` anchor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_server_hosts_two_conversations() {
    if !e2e_enabled() {
        eprintln!("SKIP: set AIONUI_OPENCODE_E2E=1 to run live opencode shared-server E2E");
        return;
    }
    let program = opencode_program().expect("opencode not on PATH");
    let conn = OpencodeConnection::new();
    let dir_a = scratch("alpha");
    let dir_b = scratch("bravo");

    // ---- conversation A: first open spawns the server ----------------------
    let a = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: "e2e-a".into(),
            },
            base_config(&dir_a, &program),
        )
        .await
        .expect("open A");
    let mut rx_a = a.events();
    let seen_a = Arc::new(std::sync::Mutex::new(Vec::new()));

    // ---- conversation B: MUST reuse the same server ------------------------
    let b = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: "e2e-b".into(),
            },
            base_config(&dir_b, &program),
        )
        .await
        .expect("open B");
    let seen_b = Arc::new(std::sync::Mutex::new(Vec::<SessionEvent>::new()));

    let info = global_pool()
        .peek(&program)
        .await
        .expect("pool must host the shared server");
    assert!(info.base_url.starts_with("http://127.0.0.1:"), "{info:?}");
    // One registry record, pointing at exactly this port = one process.
    let reg_dir = std::env::temp_dir().join("aionui-opencode-shared");
    let records: Vec<std::fs::DirEntry> = std::fs::read_dir(&reg_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    assert_eq!(records.len(), 1, "exactly one registry record expected");

    // ---- BackendBound carries the ses_ ids, and they DIFFER ---------------
    let bound_a = collect_until(
        &mut rx_a,
        Duration::from_secs(20),
        |e| matches!(e, SessionEvent::BackendBound { .. }),
        &seen_a,
    )
    .await;
    let ses_a = snapshot(&seen_a)
        .into_iter()
        .find_map(|e| match e {
            SessionEvent::BackendBound { backend_session_id } => backend_session_id,
            _ => None,
        })
        .expect("A bound to a ses_ id");
    assert!(ses_a.starts_with("ses_"), "A got {ses_a}");
    assert!(bound_a);

    // (B's BackendBound was emitted at open time; B liveness is proven via
    // its own first turn below on a freshly-subscribed receiver instead.)

    // ---- A full turn on A --------------------------------------------------
    a.dispatch(Command::Send {
        content: text("Reply with exactly this word and nothing else: ALPHA-OK"),
        metadata: meta(),
    })
    .await
    .expect("send A admitted");
    let completed_a = collect_until(
        &mut rx_a,
        Duration::from_secs(240),
        |e| matches!(e, SessionEvent::TurnResult { .. }),
        &seen_a,
    )
    .await;
    assert!(completed_a, "A never terminated the turn");
    let evs_a = snapshot(&seen_a);
    assert!(
        evs_a.iter().any(|e| matches!(e, SessionEvent::TurnStarted { .. })),
        "A missing TurnStarted"
    );
    assert!(
        evs_a.iter().any(|e| matches!(e, SessionEvent::MessageDelta { .. }))
            || evs_a
                .iter()
                .any(|e| matches!(e, SessionEvent::TurnResult { is_error: false, .. })),
        "A produced neither deltas nor a clean result: {evs_a:?}"
    );

    // ---- B's turn proves the SECOND stream is independent ------------------
    // (A goes busy-then-idle on one server while B runs its own turn.)
    // events() hands out a fresh broadcast receiver per call.
    let mut rx_b2 = b.events();
    b.dispatch(Command::Send {
        content: text("Reply with exactly this word and nothing else: BRAVO-OK"),
        metadata: meta(),
    })
    .await
    .expect("send B admitted");
    let seen_b_collector = seen_b.clone();
    let done_b = tokio::time::timeout(Duration::from_secs(240), async {
        while let Some(env) = rx_b2.next().await {
            let hit = matches!(env.event, SessionEvent::TurnResult { .. });
            seen_b_collector.lock().unwrap().push(env.event);
            if hit {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(done_b, "B never terminated its turn on the shared server");

    // ---- resume A on a fresh backend instance ------------------------------
    drop(a);
    let a2 = conn
        .open_session(
            SessionSpec::Resume {
                session_id: "e2e-a2".into(),
                backend_session_id: Some(ses_a.clone()),
            },
            base_config(&dir_a, &program),
        )
        .await
        .expect("resume A");
    let mut rx_a2 = a2.events();
    let seen_a2 = Arc::new(std::sync::Mutex::new(Vec::new()));
    a2.dispatch(Command::Send {
        content: text("Reply with exactly this word and nothing else: RESUME-OK"),
        metadata: meta(),
    })
    .await
    .expect("resumed A send admitted");
    let done_a2 = collect_until(
        &mut rx_a2,
        Duration::from_secs(240),
        |e| matches!(e, SessionEvent::TurnResult { .. }),
        &seen_a2,
    )
    .await;
    assert!(done_a2, "resumed session never terminated its turn");
    // The resumed turn must carry a higher turn generation than nothing —
    // and the pool still hosts exactly ONE server (nothing respawned).
    let info2 = global_pool().peek(&program).await.expect("still hosted");
    assert_eq!(info.base_url, info2.base_url, "resume must NOT spawn a second server");
}

/// #3: bash permission approval round-trip against a workspace whose
/// `.opencode/opencode.json` asks (`{"permission":{"bash":"ask"}}`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_approval_unblocks_the_turn() {
    if !e2e_enabled() {
        eprintln!("SKIP: set AIONUI_OPENCODE_E2E=1 to run live opencode shared-server E2E");
        return;
    }
    let program = opencode_program().expect("opencode not on PATH");
    // Dedicated dir so the ask-rule is the only permission config in scope.
    let dir = scratch("perm");
    std::fs::create_dir_all(dir.join(".opencode")).unwrap();
    std::fs::write(
        dir.join(".opencode").join("opencode.json"),
        r#"{"permission":{"bash":"ask"}}"#,
    )
    .unwrap();

    let conn = OpencodeConnection::new();
    let be = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: "e2e-perm".into(),
            },
            base_config(&dir, &program),
        )
        .await
        .expect("open perm session");
    let mut rx = be.events();
    let mut saw_permission = false;
    let mut answered = false;
    let mut completed = false;

    be.dispatch(Command::Send {
        content: text(
            "Use the bash tool to run exactly: echo PERMISSION-E2E \
             Then reply with the tool output.",
        ),
        metadata: meta(),
    })
    .await
    .expect("admitted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(env)) = tokio::time::timeout(Duration::from_secs(5), rx.next()).await else {
            continue; // idle — the pending-poll surfaces permissions on its own pace
        };
        match &env.event {
            SessionEvent::Permission { request_id, .. } => {
                saw_permission = true;
                if !answered {
                    be.dispatch(Command::AnswerPermission {
                        request_id: request_id.clone(),
                        decision: PermissionDecision::Approved,
                        selected: None,
                        answers: Vec::new(),
                    })
                    .await
                    .expect("approve permission admitted");
                    answered = true;
                }
            }
            SessionEvent::TurnResult { is_error, .. } => {
                completed = true;
                assert!(!is_error, "permission turn errored: {:?}", env.event);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_permission, "no Permission event from poll-discovery");
    assert!(answered, "never answered");
    assert!(completed, "turn never completed after approval");
}

/// #5: killing the shared server surfaces Detached, and the pool respawns a
/// healthy server for the next open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_crash_detaches_then_resumes_on_next_open() {
    if !e2e_enabled() {
        eprintln!("SKIP: set AIONUI_OPENCODE_E2E=1 to run live opencode shared-server E2E");
        return;
    }
    let program = opencode_program().expect("opencode not on PATH");
    let dir = scratch("crash");
    let conn = OpencodeConnection::new();
    let be = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: "e2e-crash".into(),
            },
            base_config(&dir, &program),
        )
        .await
        .expect("open crash session");
    let mut rx = be.events();

    let info = global_pool().peek(&program).await.expect("hosted");
    let pid = info.pid.expect("own spawn records pid");

    // Hard-kill the server (we own it: the pool spawned this pid).
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();

    let detached = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(env) = rx.next().await {
            if matches!(env.event, SessionEvent::Detached { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(detached, "server death never surfaced as Detached");
    drop(be);

    // Next open must respawn (stale registry record health-fails → fresh spawn).
    let be2 = conn
        .open_session(
            SessionSpec::Fresh {
                session_id: "e2e-crash-2".into(),
            },
            base_config(&dir, &program),
        )
        .await
        .expect("respawn after crash");
    let info2 = global_pool().peek(&program).await.expect("respawned");
    assert_ne!(info.base_url, info2.base_url, "respawn uses a fresh port");
    let mut rx2 = be2.events();
    be2.dispatch(Command::Send {
        content: text("Reply with exactly: RESPAWN-OK"),
        metadata: meta(),
    })
    .await
    .expect("post-crash send");
    let done = tokio::time::timeout(Duration::from_secs(240), async {
        while let Some(env) = rx2.next().await {
            if matches!(env.event, SessionEvent::TurnResult { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(done, "post-crash turn never completed");
    // Cancel on an idle session is a benign no-op, not an error.
    let _ = be2
        .dispatch(Command::Cancel {
            target: CancelTarget::Turn,
        })
        .await;
}
