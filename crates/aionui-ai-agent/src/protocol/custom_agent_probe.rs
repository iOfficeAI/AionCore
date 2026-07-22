//! Two-step probe for custom ACP agents.
//!
//! Step 1: `which`/`where` — resolve the first token of `command` on
//!         `$PATH`. Bounded by `execFileSync`-equivalent 5 s timeout.
//! Step 2: Spawn the CLI via `CliAgentProcess::spawn_for_sdk`, connect
//!         an `AcpProtocol` (which owns the ACP `initialize` handshake
//!         with a built-in 30 s timeout), then shut down cleanly.
//!
//! The same function is called by:
//!   - `POST /api/agents/custom/try-connect`  (manual "test connection" button)
//!   - `AgentService::create/update_custom_agent`   (test-on-save)
//!
//! Both paths produce identical outcomes / error text.

use std::collections::HashMap;
use std::time::Duration;

use aionui_api_types::TryConnectCustomAgentResponse;
use aionui_common::{CommandSpec, EnvVar};
use aionui_runtime::{NodeRuntimeProgressReporter, ResolvedCommand, ensure_runtime_command_with_reporter};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::capability::cli_process::CliAgentProcess;
use crate::protocol::acp::AcpProtocol;
use crate::protocol::error::AcpError;
use crate::protocol::npx_cache_repair::CorruptNpxCacheRepair;

use agent_client_protocol::schema::NewSessionRequest;

/// Step 2 overall timeout. Belt-and-suspenders: `AcpProtocol::connect`
/// already caps the initialize RPC at 30 s, but a CLI that hangs
/// before writing any ACP frame at all is covered by this outer cap.
const STEP2_TIMEOUT: Duration = Duration::from_secs(35);

/// Grace period for the child to exit on its own after stdin close, before
/// we fall back to SIGKILL on the whole process group. Keep this short because
/// manual connection tests should return promptly after the ACP probe finishes.
const PROBE_KILL_GRACE: Duration = Duration::from_millis(500);

/// Probe a custom ACP agent.
///
/// Returns `Success` only if both `which` and the ACP `initialize`
/// handshake succeed. Any failure short-circuits into the
/// corresponding variant.
pub async fn try_connect_custom_agent(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> TryConnectCustomAgentResponse {
    // ── Step 1 — which check ────────────────────────────────────────
    let head = first_token(command);
    let resolved = match ensure_runtime_command_with_reporter(head, reporter).await {
        Ok(resolved) => resolved,
        Err(error) => {
            return TryConnectCustomAgentResponse::FailCli {
                error: error.to_string(),
            };
        }
    };
    debug!(program = %resolved.program.display(), "probe step 1 ok");

    // ── Step 2 — spawn + ACP initialize ─────────────────────────────
    let proc = match spawn_probe_process(resolved, args, env).await {
        Ok(proc) => proc,
        Err(msg) => return TryConnectCustomAgentResponse::FailAcp { error: msg },
    };

    let outcome = match tokio::time::timeout(STEP2_TIMEOUT, run_handshake(&proc)).await {
        Ok(outcome) => outcome.into_response(),
        Err(_) => TryConnectCustomAgentResponse::FailAcp {
            error: format!("ACP handshake did not complete within {}s", STEP2_TIMEOUT.as_secs()),
        },
    };

    // Always tear down the whole process group. `kill_on_drop(true)` only
    // signals the direct child (e.g. `npm exec ...`) — wrapper CLIs spawn
    // grandchildren (`openclaw-acp`) that survive unless we SIGKILL the
    // group explicitly via `proc.kill()`.
    if let Err(error) = proc.kill(PROBE_KILL_GRACE).await {
        warn!(pid = proc.pid(), error = %error, "probe failed to kill process group");
    }

    outcome
}

fn first_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

async fn spawn_probe_process(
    resolved: ResolvedCommand,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<CliAgentProcess, String> {
    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVar> = env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    let spec = CommandSpec {
        command: resolved.program,
        args: final_args,
        env: final_env,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    };

    CliAgentProcess::spawn_for_sdk(spec)
        .await
        .map_err(|e| format!("spawn failed: {e}"))
}

/// RAII guard that force-kills a probe's process tree when dropped.
///
/// Probe connections are throwaway, and some callers wrap this future in a
/// `tokio::time::timeout`. On any exit — success, `?` early-return, or
/// cancellation when the timeout fires and drops the future — we must reap the
/// whole spawned process group. `kill_on_drop` only reaps the direct child, so
/// an npx-spawned ACP grandchild (`codex-acp`, `codebuddy --acp`, …) would
/// otherwise reparent to init and leak as an orphan.
struct ProbeProcessGuard<'a> {
    proc: &'a CliAgentProcess,
}

impl Drop for ProbeProcessGuard<'_> {
    fn drop(&mut self) {
        self.proc.force_kill_tree();
    }
}

/// Probe a pre-built [`CommandSpec`] (used by the builtin managed-agent path).
///
/// Runs the same Step 2 handshake as [`try_connect_custom_agent`] —
/// `initialize` followed by `session/new` — so auth-gated builtin agents
/// (e.g. gemini logged out) surface as [`TryConnectCustomAgentResponse::FailAuth`]
/// rather than appearing online.
pub(crate) async fn acp_probe_command_spec(spec: CommandSpec) -> TryConnectCustomAgentResponse {
    let mut corrupt_npx_cache_repair = CorruptNpxCacheRepair::default();

    loop {
        match acp_probe_command_spec_once(spec.clone()).await {
            ProbeOutcome::Fail(error) => {
                if let Some(cache_entry) = corrupt_npx_cache_repair.try_repair(&spec, &error) {
                    tracing::info!(
                        npm_npx_cache_entry = %cache_entry.display(),
                        "Cleared corrupt npm npx cache after ACP probe startup crash; retrying probe once"
                    );
                    continue;
                }

                return TryConnectCustomAgentResponse::FailAcp { error };
            }
            outcome => return outcome.into_response(),
        }
    }
}

async fn acp_probe_command_spec_once(spec: CommandSpec) -> ProbeOutcome {
    let proc = match CliAgentProcess::spawn_for_sdk(spec).await {
        Ok(proc) => proc,
        Err(e) => return ProbeOutcome::Fail(format!("spawn failed: {e}")),
    };

    // From here on, the process tree is reaped on every exit path, including
    // cancellation when an outer timeout drops this future.
    let _guard = ProbeProcessGuard { proc: &proc };

    run_handshake(&proc).await
}

/// Result of the Step 2 probe (`initialize` + `session/new`).
///
/// The probe reaches `session/new` so it can tell "reachable but not
/// authorized" (`Auth`) apart from other ACP failures (`Fail`) — `initialize`
/// alone returns `authMethods` even for already-authorized agents and cannot
/// make this distinction.
enum ProbeOutcome {
    Ok,
    Auth(String),
    Fail(String),
}

impl ProbeOutcome {
    fn into_response(self) -> TryConnectCustomAgentResponse {
        match self {
            ProbeOutcome::Ok => TryConnectCustomAgentResponse::Success,
            ProbeOutcome::Auth(error) => TryConnectCustomAgentResponse::FailAuth { error },
            ProbeOutcome::Fail(error) => TryConnectCustomAgentResponse::FailAcp { error },
        }
    }
}

async fn run_handshake(proc: &CliAgentProcess) -> ProbeOutcome {
    let Some((stdin, stdout)) = proc.take_stdio().await else {
        return ProbeOutcome::Fail("stdio not available after spawn_for_sdk".to_string());
    };

    // Throwaway channels — a probe session never sends a prompt, so no events,
    // permission requests, or notifications are consumed.
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (permission_tx, _permission_rx) = mpsc::channel(4);
    let (notification_tx, _notification_rx) = mpsc::channel(4);

    // Race the ACP initialize handshake against the child process exiting.
    // A misconfigured CLI (e.g. an invalid package launcher command) exits
    // almost immediately with a non-zero status; without this race the
    // `AcpProtocol::connect` call would block on its internal 30 s
    // timeout waiting for an `initialize` reply that will never arrive.
    let connect = AcpProtocol::connect(stdin, stdout, event_tx, permission_tx, notification_tx);
    let protocol = tokio::select! {
        biased;
        res = connect => match res {
            Ok(protocol) => protocol,
            Err(e) => return ProbeOutcome::Fail(format!("ACP initialize failed: {e}")),
        },
        exit = proc.wait_for_exit() => {
            let stderr = proc.take_stderr().await;
            let stderr = stderr.trim();
            let status = match exit {
                Some(s) => format!("{s}"),
                None => "unknown".to_string(),
            };
            return if stderr.is_empty() {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status})"))
            } else {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status}): {stderr}"))
            };
        }
    };

    // `initialize` only proves the agent speaks ACP, not that it is usable.
    // Open a real session (no prompt) so an auth-gated agent surfaces its
    // `auth_required` error here instead of silently appearing "online".
    let outcome = match protocol.new_session(NewSessionRequest::new(std::env::temp_dir())).await {
        Ok(_) => ProbeOutcome::Ok,
        Err(AcpError::AuthRequired) => {
            ProbeOutcome::Auth("Agent reachable but requires login/authorization".to_string())
        }
        Err(e) => ProbeOutcome::Fail(format!("ACP session/new failed: {e}")),
    };

    // Drop `protocol` so its shutdown oneshot fires before the outer cleanup
    // path (or the drop guard for timeout-cancelled callers) reaps the process
    // tree. The probe session is throwaway; the process-group kill in the
    // caller tears down the session along with the CLI.
    drop(protocol);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn probe_returns_fail_cli_when_command_missing() {
        let resp = try_connect_custom_agent("aionui-definitely-does-not-exist-xyz", &[], &HashMap::new(), None).await;
        match resp {
            TryConnectCustomAgentResponse::FailCli { error } => {
                let lower = error.to_lowercase();
                assert!(
                    lower.contains("not found") || lower.contains("no such") || lower.contains("was not found"),
                    "expected 'not found' style message, got: {error}"
                );
            }
            other => panic!("expected FailCli, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reaps_grandchild_when_caller_timeout_cancels_handshake() {
        // The CLI backgrounds a long-lived grandchild then hangs without ever
        // speaking ACP, so the handshake never completes. The caller wraps the
        // probe in a tight timeout; when it fires the probe future is dropped
        // mid-flight. The drop guard must still reap the whole process group,
        // including the reparented grandchild (the production orphan leak).
        use aionui_common::{CommandSpec, EnvVar};

        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_string_lossy().into_owned();

        let spec = CommandSpec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                // Background a sleeper, record its pid, then block forever
                // while keeping stdout open and silent so ACP initialize never
                // gets a response (we must NOT echo stdin — that would look
                // like a malformed reply and make `connect` fail fast).
                "sleep 60 & printf '%s' \"$!\" > \"$1\"; exec sleep 60".into(),
                "probe-timeout-cleanup".into(),
                marker_path.clone(),
            ],
            env: Vec::<EnvVar>::new(),
            cwd: None,
        };

        // Warm the lazily-loaded shell-env cache so the spawn below is not
        // racing a cold login-shell capture against our timeout.
        let _ = aionui_runtime::agent_process_env().await;

        let result = tokio::time::timeout(Duration::from_secs(2), acp_probe_command_spec(spec)).await;
        assert!(
            result.is_err(),
            "handshake should not complete; outer timeout must fire"
        );

        let child_pid: u32 = {
            // The marker is written very early; poll briefly in case of races.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(contents) = std::fs::read_to_string(marker.path())
                    && let Ok(pid) = contents.trim().parse::<u32>()
                {
                    break pid;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "grandchild pid marker never appeared"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };

        fn is_pid_alive(pid: u32) -> bool {
            let rc = unsafe { libc::kill(pid as i32, 0) };
            if rc == 0 {
                return true;
            }
            !matches!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH))
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while is_pid_alive(child_pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !is_pid_alive(child_pid),
            "grandchild pid={child_pid} should be reaped after the probe future is cancelled",
        );
    }

    #[tokio::test]
    async fn probe_returns_fail_acp_when_command_is_noop() {
        // `true` exits 0 immediately — Step 1 passes (on PATH), but the
        // process dies before ACP initialize completes, so Step 2 maps
        // to FailAcp.
        if cfg!(windows) {
            // `true` is a cmd builtin on Windows, not a standalone exe.
            return;
        }
        let resp = try_connect_custom_agent("true", &[], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "expected FailAcp, got {resp:?}"
        );
    }

    /// Regression for the production leak: a probe that talks to a wrapper
    /// CLI (`npm exec ...`, etc.) historically left the wrapper's grandchild
    /// process alive when the probe returned, because cleanup relied on
    /// `kill_on_drop(true)` which only signals the direct child. Repeated
    /// connection tests could otherwise accumulate zombie `openclaw-acp`
    /// processes.
    ///
    /// We exercise the public entry point with a CLI that exits immediately
    /// after backgrounding a long-lived grandchild — that's the production
    /// shape `npm exec openclaw --acp` collapses into when its own ACP
    /// handshake fails. The probe will see the wrapper exit (ACP fail), but
    /// by that point the grandchild has been forked. The fix must SIGKILL
    /// the whole process group before returning, so the grandchild dies too.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_kills_grandchild_left_behind_by_wrapper() {
        use std::time::Duration;
        use tokio::time::Instant;

        fn is_pid_alive(pid: i32) -> bool {
            unsafe { libc::kill(pid, 0) == 0 }
        }

        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_owned();
        // Background a grandchild, write its pid, then exit. The probe will
        // observe `proc.wait_for_exit()` race the ACP handshake and return
        // FailAcp — the grandchild keeps running unless cleanup kills it.
        let script = format!("sleep 600 & printf '%s' \"$!\" > '{}'", marker_path.display());

        let resp = try_connect_custom_agent("sh", &["-c".to_string(), script], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "wrapper exits before ACP handshake; expected FailAcp, got {resp:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut marker_contents = String::new();
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&marker_path)
                && !s.trim().is_empty()
            {
                marker_contents = s;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild_pid: i32 = marker_contents.trim().parse().unwrap_or_else(|_| {
            panic!("wrapper did not write the grandchild pid: {marker_contents:?}");
        });

        // Give the OS a brief moment to reap after the probe returned.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_pid_alive(grandchild_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Cleanup so a failing test does not leave an actual leak.
        unsafe {
            libc::kill(grandchild_pid, libc::SIGKILL);
        }
        panic!("grandchild pid={grandchild_pid} survived the probe — process group cleanup is broken");
    }
}
