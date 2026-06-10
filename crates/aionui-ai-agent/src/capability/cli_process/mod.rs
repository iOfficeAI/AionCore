use std::fs;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, watch};
use tracing::{debug, warn};

use crate::error::AgentError;

mod spawn_sdk;
mod stderr_monitor;

use stderr_monitor::force_kill;

/// Maximum stderr ring-buffer size in bytes.
pub(super) const STDERR_BUFFER_MAX: usize = 8192;

/// Trim `buf` from the front so it holds at most the last `max` bytes.
///
/// The raw cut point can land inside a multi-byte UTF-8 character, where
/// `String::drain` would panic; rounding up to the next char boundary keeps
/// the buffer valid and never exceeds `max` (issue #392).
pub(super) fn trim_to_tail(buf: &mut String, max: usize) {
    if buf.len() > max {
        let cut = buf.ceil_char_boundary(buf.len() - max);
        buf.drain(..cut);
    }
}

pub(super) fn prepare_command_cwd(cwd: &str) -> Result<PathBuf, AgentError> {
    if cwd.trim().is_empty() {
        return Err(AgentError::bad_request("Workspace directory is empty"));
    }

    let workspace_path = PathBuf::from(cwd);
    match fs::metadata(&workspace_path) {
        Ok(metadata) if metadata.is_dir() => Ok(workspace_path),
        Ok(_) | Err(_) => Err(AgentError::workspace_path_runtime_unavailable(
            workspace_path.display().to_string(),
        )),
    }
}

/// Manages a CLI subprocess used by SDK-based agent transports.
///
/// ACP sessions call [`take_stdio`](Self::take_stdio) to hand raw stdin/stdout
/// to the ACP SDK transport. Aionrs uses its own manager and does not rely on
/// line-delimited legacy JSON mode.
pub struct CliAgentProcess {
    /// Stdin writer, wrapped in Mutex for concurrent send safety.
    /// Set to `None` once stdin is closed, taken, or process exited.
    stdin: Mutex<Option<ChildStdin>>,
    /// Raw stdout handle. Only available before background tasks start or
    /// in SDK mode (taken by `take_stdio`). `None` once consumed.
    stdout: Mutex<Option<ChildStdout>>,
    /// OS-level process ID.
    pid: u32,
    /// Process group ID captured at spawn time so teardown can still target
    /// the whole tree after the direct child exits.
    process_group_id: Option<u32>,
    /// Watch channel that transitions from `None` → `Some(ExitStatus)` on exit.
    exit_rx: watch::Receiver<Option<ExitStatus>>,
    /// Stderr ring buffer for diagnostics.
    #[allow(dead_code)] // Read via take_stderr(); part of diagnostics API for startup crash reporting
    stderr_buffer: Arc<Mutex<String>>,
    /// Handle to the stderr reader task (for cleanup).
    _stderr_handle: Arc<tokio::task::JoinHandle<()>>,
    /// Handle to the exit monitor task (for cleanup).
    _exit_handle: Arc<tokio::task::JoinHandle<()>>,
}

impl CliAgentProcess {
    /// Take ownership of stdin and stdout for the SDK transport.
    ///
    /// Only available in SDK mode (after [`spawn_for_sdk`](Self::spawn_for_sdk)).
    /// Can only be called once. Returns `None` on subsequent calls or if
    /// spawned in legacy mode.
    pub async fn take_stdio(&self) -> Option<(ChildStdin, ChildStdout)> {
        let stdin = self.stdin.lock().await.take()?;
        let stdout = self.stdout.lock().await.take()?;
        Some((stdin, stdout))
    }

    /// Close stdin, signaling the subprocess that no more input will arrive.
    pub async fn close_stdin(&self) {
        let mut guard = self.stdin.lock().await;
        if guard.take().is_some() {
            debug!(pid = self.pid, "Stdin closed");
        }
    }

    /// Gracefully terminate the subprocess.
    ///
    /// 1. Close stdin
    /// 2. Wait up to `grace_period` for the process to exit on its own
    /// 3. If still running after grace period, send SIGKILL
    pub async fn kill(&self, grace_period: Duration) -> Result<(), AgentError> {
        // Close stdin first to signal the child
        self.close_stdin().await;

        // Wait for graceful exit within the grace period
        let mut rx = self.exit_rx.clone();
        let exited = tokio::time::timeout(grace_period, async {
            // If already exited, return immediately
            if rx.borrow().is_some() {
                return;
            }
            // Wait for state change
            let _ = rx.changed().await;
        })
        .await;

        if exited.is_ok() && self.exit_rx.borrow().is_some() {
            debug!(pid = self.pid, "CLI process exited gracefully");
            return Ok(());
        }

        // Force kill
        warn!(pid = self.pid, "Grace period expired, sending SIGKILL");
        force_kill(self.pid, self.process_group_id)?;

        // Wait for the exit monitor to observe process termination so callers
        // do not race a still-live child after force-kill returns.
        let mut rx = self.exit_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            if rx.borrow().is_some() {
                return;
            }
            let _ = rx.changed().await;
        })
        .await
        .map_err(|_| AgentError::internal(format!("Process {} did not exit after force_kill", self.pid)))?;

        Ok(())
    }

    /// Check whether the subprocess is still running.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn is_running(&self) -> bool {
        self.exit_rx.borrow().is_none()
    }

    /// Get the exit status if the process has exited.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn exit_status(&self) -> Option<ExitStatus> {
        *self.exit_rx.borrow()
    }

    /// Get the OS process ID.
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Get the cached process group ID captured when the child was spawned.
    pub fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    /// Wait for the process to exit (blocks until exit or cancellation).
    #[allow(dead_code)] // Complete CliProcess lifecycle API
    pub async fn wait_for_exit(&self) -> Option<ExitStatus> {
        let mut rx = self.exit_rx.clone();
        // If already exited, return immediately
        if let Some(status) = *rx.borrow() {
            return Some(status);
        }
        // Wait for state change
        let _ = rx.changed().await;
        *rx.borrow()
    }

    /// Take the buffered stderr content (consuming).
    ///
    /// Returns the last [`STDERR_BUFFER_MAX`] bytes of stderr output.
    /// Used for error diagnostics in `AcpError::StartupCrash` and
    /// `AcpError::Disconnected`.
    #[allow(dead_code)] // Diagnostics API for startup crash and disconnect error reporting
    pub async fn take_stderr(&self) -> String {
        let mut buf = self.stderr_buffer.lock().await;
        std::mem::take(&mut *buf)
    }

    /// Clear the buffered stderr ring so subsequent peeks only observe lines
    /// written after the caller opens a new diagnostics window.
    pub async fn clear_stderr(&self) {
        let _ = self.take_stderr().await;
    }

    /// Peek the last `max_lines` newline-delimited lines from the stderr ring
    /// buffer **without draining**.
    ///
    /// Used by error-augmentation paths (`AcpAgentManager::send_message`) that
    /// need to surface tracing-level error context the SDK didn't include in
    /// its JSON-RPC response. Returns an owned `String`; the buffer lock is
    /// held for the duration of this call (microseconds at the bounded sizes
    /// we read) and dropped before the result is returned.
    ///
    /// `max_lines == 0` returns an empty string. The returned string has no
    /// trailing newline — the caller may append one if they want.
    #[allow(dead_code)] // Called by error-augmentation path in AcpAgentManager::send_message (Task 5)
    pub async fn peek_stderr_tail(&self, max_lines: usize) -> String {
        if max_lines == 0 {
            return String::new();
        }
        let buf = self.stderr_buffer.lock().await;
        let trimmed = buf.trim_end_matches('\n');
        if trimmed.is_empty() {
            return String::new();
        }
        // `rsplit('\n')` walks lines from the end. Take up to `max_lines`,
        // then re-collect into the original top-to-bottom order.
        let mut tail: Vec<&str> = trimmed.rsplit('\n').take(max_lines).collect();
        tail.reverse();
        tail.join("\n")
    }
}

#[cfg(unix)]
pub(super) fn tracked_process_group_id(pid: u32) -> Option<u32> {
    Some(pid)
}

#[cfg(not(unix))]
pub(super) fn tracked_process_group_id(_pid: u32) -> Option<u32> {
    None
}

#[cfg(test)]
pub(super) mod tests {
    use aionui_common::CommandSpec;

    use super::*;
    use tokio::time::timeout;

    pub(super) fn simple_script_config(script: &str) -> CommandSpec {
        CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            cwd: None,
        }
    }

    pub(super) async fn spawn_sdk_test_process(config: CommandSpec) -> CliAgentProcess {
        let data_dir = tempfile::tempdir().unwrap();
        CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await.unwrap()
    }

    // ── trim_to_tail ─────────────────────────────────────────────────

    #[test]
    fn trim_to_tail_does_not_panic_when_cut_lands_inside_multibyte_char() {
        // "ab" (2 bytes) + "中中中" (9 bytes) = 11 bytes; max 8 puts the raw
        // cut point at byte 3, inside the first '中' (bytes 2..5).
        let mut buf = String::from("ab中中中");
        trim_to_tail(&mut buf, 8);
        assert_eq!(buf, "中中");
        assert!(buf.len() <= 8);
    }

    #[test]
    fn trim_to_tail_keeps_exactly_max_bytes_for_ascii() {
        let mut buf = String::from("abcdefghij");
        trim_to_tail(&mut buf, 8);
        assert_eq!(buf, "cdefghij");
    }

    #[test]
    fn trim_to_tail_is_noop_when_under_max() {
        let mut buf = String::from("short");
        trim_to_tail(&mut buf, 8);
        assert_eq!(buf, "short");
    }

    #[test]
    fn trim_to_tail_never_exceeds_max_with_emoji_flood() {
        // Regression for the production panic: emoji-rich stderr lines pushed
        // the buffer over STDERR_BUFFER_MAX with cut points off-boundary.
        let mut buf = String::new();
        for _ in 0..600 {
            buf.push_str("⚠️ API call failed 🌐\n");
        }
        let original = buf.clone();
        trim_to_tail(&mut buf, STDERR_BUFFER_MAX);
        assert!(buf.len() <= STDERR_BUFFER_MAX);
        assert!(original.ends_with(buf.as_str()));
    }

    // ── Lifecycle tests (apply to both modes) ────────────────────────

    #[tokio::test]
    async fn is_running_reflects_process_state() {
        let config = simple_script_config("sleep 10");
        let proc = spawn_sdk_test_process(config).await;

        assert!(proc.is_running());
        assert!(proc.exit_status().is_none());

        proc.kill(Duration::from_millis(100)).await.unwrap();

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        assert!(!proc.is_running());
        assert!(proc.exit_status().is_some());
    }

    #[tokio::test]
    async fn kill_with_grace_period_exits_cleanly() {
        let config = simple_script_config("read line");
        let proc = spawn_sdk_test_process(config).await;
        assert!(proc.is_running());

        proc.kill(Duration::from_secs(5)).await.unwrap();
        assert!(!proc.is_running());
    }

    #[tokio::test]
    async fn kill_force_kills_after_grace_period() {
        let config = simple_script_config("trap '' TERM; while true; do sleep 1; done");
        let proc = spawn_sdk_test_process(config).await;
        assert!(proc.is_running());

        let result = proc.kill(Duration::from_millis(100)).await;
        assert!(result.is_ok());

        timeout(Duration::from_secs(5), proc.wait_for_exit()).await.unwrap();
        assert!(!proc.is_running());
    }

    #[tokio::test]
    async fn spawn_rejects_unavailable_cwd_with_trailing_whitespace_in_request() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("workspace");
        fs::create_dir(&cwd).unwrap();
        let cwd_with_trailing_space = format!("{} ", cwd.to_string_lossy());

        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(cwd_with_trailing_space.clone()),
        };
        let data_dir = tempfile::tempdir().unwrap();
        let result = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await;
        assert!(matches!(
            result,
            Err(AgentError::WorkspacePathRuntimeUnavailable(message)) if message == cwd_with_trailing_space
        ));
    }

    #[tokio::test]
    async fn spawn_allows_cwd_with_whitespace_in_any_segment() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_parent = dir.path().join("my workspace");
        fs::create_dir(&workspace_parent).unwrap();
        let cwd = workspace_parent.join("project");
        fs::create_dir(&cwd).unwrap();

        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(cwd.to_string_lossy().into_owned()),
        };

        let proc = spawn_sdk_test_process(config).await;
        proc.kill(Duration::from_millis(100)).await.unwrap();
    }

    #[tokio::test]
    async fn spawn_for_sdk_allows_cwd_with_whitespace_in_any_segment() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_parent = dir.path().join("my workspace");
        fs::create_dir(&workspace_parent).unwrap();
        let cwd = workspace_parent.join("project");
        fs::create_dir(&cwd).unwrap();
        let data_dir = tempfile::tempdir().unwrap();

        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "echo ready".into()],
            env: vec![],
            cwd: Some(cwd.to_string_lossy().into_owned()),
        };

        let proc = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await.unwrap();
        proc.kill(Duration::from_millis(100)).await.unwrap();
    }

    #[tokio::test]
    async fn spawn_rejects_missing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let missing_cwd = dir.path().join("missing").join("workspace");
        assert!(!missing_cwd.exists());

        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "echo \"{\\\"cwd\\\":\\\"$PWD\\\"}\"".into()],
            env: vec![],
            cwd: Some(missing_cwd.to_string_lossy().into_owned()),
        };

        let data_dir = tempfile::tempdir().unwrap();
        let result = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await;
        assert!(matches!(
            result,
            Err(AgentError::WorkspacePathRuntimeUnavailable(message))
                if message == missing_cwd.to_string_lossy()
        ));
        assert!(!missing_cwd.exists());
    }

    #[tokio::test]
    async fn spawn_for_sdk_rejects_missing_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let missing_cwd = dir.path().join("missing-sdk").join("workspace");
        assert!(!missing_cwd.exists());

        let config = CommandSpec {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 10".into()],
            env: vec![],
            cwd: Some(missing_cwd.to_string_lossy().into_owned()),
        };

        let result = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await;
        assert!(matches!(
            result,
            Err(AgentError::WorkspacePathRuntimeUnavailable(message))
                if message == missing_cwd.to_string_lossy()
        ));
        assert!(!missing_cwd.exists());
    }

    #[tokio::test]
    async fn spawn_invalid_command_returns_error() {
        let config = CommandSpec {
            command: "/nonexistent/binary/that/does/not/exist".into(),
            args: vec![],
            env: vec![],
            cwd: None,
        };
        let data_dir = tempfile::tempdir().unwrap();
        let result = CliAgentProcess::spawn_for_sdk(config, data_dir.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn pid_is_nonzero_for_valid_process() {
        let config = simple_script_config("sleep 10");
        let proc = spawn_sdk_test_process(config).await;
        assert!(proc.pid() > 0);
        proc.kill(Duration::from_millis(100)).await.unwrap();
    }

    #[tokio::test]
    async fn wait_for_exit_returns_immediately_if_already_exited() {
        let config = simple_script_config("true");
        let proc = spawn_sdk_test_process(config).await;

        let status1 = timeout(Duration::from_secs(5), proc.wait_for_exit())
            .await
            .expect("Timed out");
        assert!(status1.is_some());

        let status2 = timeout(Duration::from_millis(100), proc.wait_for_exit())
            .await
            .expect("Should return immediately");
        assert!(status2.is_some());
    }
}
