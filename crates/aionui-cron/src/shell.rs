//! Native shell execution for shell-action cron jobs.
//!
//! A shell-action job runs its command via the system shell with the backend
//! process as parent — no agent, no model tokens. Output is captured with a
//! per-stream cap so a runaway command cannot balloon memory, and only the
//! tail of each stream is reported back (errors surface at the end).

use aionui_runtime::Builder;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

use crate::error::CronError;

pub const DEFAULT_SHELL_TIMEOUT_MS: i64 = 600_000;
pub const MIN_SHELL_TIMEOUT_MS: i64 = 1_000;
pub const MAX_SHELL_TIMEOUT_MS: i64 = 3_600_000;
/// Per-stream cap for the captured output kept in memory.
const MAX_CAPTURED_BYTES: usize = 64 * 1024;
/// Per-stream cap for the tail reported in the result message.
const MAX_REPORTED_CHARS: usize = 4_000;

/// Result of one shell run. `stdout_tail`/`stderr_tail` hold at most
/// [`MAX_REPORTED_CHARS`] characters each, prefixed with a truncation marker
/// when bytes were dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub duration_ms: i64,
}

impl ShellOutcome {
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

pub fn effective_timeout_ms(shell_timeout_ms: Option<i64>) -> i64 {
    match shell_timeout_ms {
        Some(value) => value.clamp(MIN_SHELL_TIMEOUT_MS, MAX_SHELL_TIMEOUT_MS),
        None => DEFAULT_SHELL_TIMEOUT_MS,
    }
}

pub fn validate_shell_command(command: &str) -> Result<(), CronError> {
    if command.trim().is_empty() {
        return Err(CronError::InvalidShellAction(
            "shell action requires a non-empty command in the message field".into(),
        ));
    }
    Ok(())
}

pub fn validate_shell_timeout(shell_timeout_ms: Option<i64>) -> Result<(), CronError> {
    let Some(value) = shell_timeout_ms else {
        return Ok(());
    };
    if !(MIN_SHELL_TIMEOUT_MS..=MAX_SHELL_TIMEOUT_MS).contains(&value) {
        return Err(CronError::InvalidShellAction(format!(
            "shell_timeout_ms must be between {MIN_SHELL_TIMEOUT_MS} and {MAX_SHELL_TIMEOUT_MS}, got {value}"
        )));
    }
    Ok(())
}

/// Runs `command` in `cwd` via the system shell, enforcing `timeout_ms`.
///
/// The caller is responsible for validating `cwd` exists; a missing directory
/// fails the spawn and is reported as a failed run.
pub async fn run_shell_command(command: &str, cwd: &Path, timeout_ms: i64) -> ShellOutcome {
    let started = Instant::now();
    let mut builder = shell_builder(command);
    builder.current_dir(cwd);

    let mut child = match builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ShellOutcome {
                exit_code: None,
                timed_out: false,
                stdout_tail: String::new(),
                stderr_tail: format!("failed to spawn shell: {error}"),
                duration_ms: elapsed_ms(started),
            };
        }
    };

    let stdout_reader = spawn_tail_reader(child.stdout.take());
    let stderr_reader = spawn_tail_reader(child.stderr.take());

    let wait = child.wait();
    let (status, timed_out) = match tokio::time::timeout(Duration::from_millis(timeout_ms.max(0) as u64), wait).await {
        Ok(result) => match result {
            Ok(status) => (Some(status), false),
            Err(error) => {
                let captured_stderr = join_tail(stderr_reader).await;
                let stdout_tail = join_tail(stdout_reader).await;
                let stderr_tail = if captured_stderr.trim().is_empty() {
                    format!("failed to wait for shell process: {error}")
                } else {
                    format!("failed to wait for shell process: {error}\n--- stderr ---\n{captured_stderr}")
                };
                return ShellOutcome {
                    exit_code: None,
                    timed_out: false,
                    stdout_tail,
                    stderr_tail,
                    duration_ms: elapsed_ms(started),
                };
            }
        },
        Err(_) => {
            // kill_on_drop only kills the direct shell; kill the whole tree so
            // grandchildren (e.g. a `sleep` under `bash -c`) cannot outlive the
            // timeout. The pipe readers see EOF once the tree is gone.
            let _ = aionui_runtime::kill_process_tree(&mut child).await;
            (None, true)
        }
    };

    let stdout_tail = join_tail(stdout_reader).await;
    let stderr_tail = join_tail(stderr_reader).await;

    ShellOutcome {
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        stdout_tail,
        stderr_tail,
        duration_ms: elapsed_ms(started),
    }
}

/// Builds the human-readable result message inserted into the bound
/// conversation, plus the tips type (`success` / `error`).
pub fn build_result_message(command_description: &str, outcome: &ShellOutcome) -> (String, &'static str) {
    let duration_secs = outcome.duration_ms / 1000;
    if outcome.timed_out {
        let mut message =
            format!("Scheduled shell task timed out and was killed after {duration_secs}s: {command_description}\n");
        append_stream_tail(&mut message, "stderr", &outcome.stderr_tail);
        append_stream_tail(&mut message, "stdout", &outcome.stdout_tail);
        (message.trim_end().to_owned(), "error")
    } else if outcome.succeeded() {
        let mut message =
            format!("Scheduled shell task completed in {duration_secs}s (exit 0): {command_description}\n");
        append_stream_tail(&mut message, "output", &outcome.stdout_tail);
        (message.trim_end().to_owned(), "success")
    } else {
        let exit_label = outcome
            .exit_code
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "no exit code".to_owned());
        let mut message =
            format!("Scheduled shell task failed ({exit_label}) after {duration_secs}s: {command_description}\n");
        append_stream_tail(&mut message, "stderr", &outcome.stderr_tail);
        append_stream_tail(&mut message, "stdout", &outcome.stdout_tail);
        (message.trim_end().to_owned(), "error")
    }
}

fn append_stream_tail(message: &mut String, label: &str, tail: &str) {
    if tail.trim().is_empty() {
        return;
    }
    message.push_str("\n--- ");
    message.push_str(label);
    message.push_str(" ---\n");
    message.push_str(tail);
}

fn shell_builder(command: &str) -> Builder {
    #[cfg(unix)]
    {
        let mut builder = Builder::clean_cli("bash");
        builder.arg("-c").arg(command);
        builder
    }
    #[cfg(windows)]
    {
        let _ = command;
        let mut builder = Builder::clean_cli("cmd");
        builder.arg("/C").arg(command);
        builder
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
        compile_error!("shell cron actions require unix or windows");
    }
}

fn elapsed_ms(started: Instant) -> i64 {
    started.elapsed().as_millis().min(i64::MAX as u128) as i64
}

/// Reads a pipe to completion while retaining only the last
/// [`MAX_CAPTURED_BYTES`] bytes; returns the decoded, tail-truncated text.
fn spawn_tail_reader(
    pipe: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
) -> Option<tokio::task::JoinHandle<String>> {
    pipe.map(|mut pipe| {
        tokio::spawn(async move {
            let mut tail: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8 * 1024];
            let mut dropped_bytes: usize = 0;
            loop {
                match pipe.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(read) => {
                        dropped_bytes += overflow(&mut tail, &chunk[..read]);
                    }
                    Err(_) => break,
                }
            }
            decode_tail(&tail, dropped_bytes)
        })
    })
}

/// Appends `chunk` to `tail`, keeping at most [`MAX_CAPTURED_BYTES`] bytes of
/// the combined stream and returning how many bytes were discarded.
fn overflow(tail: &mut Vec<u8>, chunk: &[u8]) -> usize {
    let total = tail.len() + chunk.len();
    if total <= MAX_CAPTURED_BYTES {
        tail.extend_from_slice(chunk);
        return 0;
    }
    let dropped = total - MAX_CAPTURED_BYTES;
    if dropped <= tail.len() {
        tail.drain(..dropped);
        tail.extend_from_slice(chunk);
    } else {
        let skip_in_chunk = dropped - tail.len();
        tail.clear();
        tail.extend_from_slice(&chunk[skip_in_chunk..]);
    }
    dropped
}

fn decode_tail(bytes: &[u8], dropped_bytes: usize) -> String {
    let mut text = String::from_utf8_lossy(bytes).to_string();
    if let Some(position) = text.find('\0') {
        text.truncate(position);
    }
    let char_count = text.chars().count();
    if char_count > MAX_REPORTED_CHARS {
        let skip = char_count - MAX_REPORTED_CHARS;
        text = text.chars().skip(skip).collect();
    }
    if dropped_bytes > 0 || char_count > MAX_REPORTED_CHARS {
        text = format!("…(earlier output truncated)…\n{text}");
    }
    text
}

async fn join_tail(reader: Option<tokio::task::JoinHandle<String>>) -> String {
    match reader {
        Some(handle) => handle.await.unwrap_or_default(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_timeout_clamps() {
        assert_eq!(effective_timeout_ms(None), DEFAULT_SHELL_TIMEOUT_MS);
        assert_eq!(effective_timeout_ms(Some(500)), MIN_SHELL_TIMEOUT_MS);
        assert_eq!(effective_timeout_ms(Some(10_000)), 10_000);
        assert_eq!(effective_timeout_ms(Some(i64::MAX)), MAX_SHELL_TIMEOUT_MS);
    }

    #[test]
    fn validate_shell_command_rejects_empty() {
        assert!(validate_shell_command("").is_err());
        assert!(validate_shell_command("   \n\t ").is_err());
        assert!(validate_shell_command("echo hi").is_ok());
    }

    #[test]
    fn validate_shell_timeout_rejects_out_of_range() {
        assert!(validate_shell_timeout(None).is_ok());
        assert!(validate_shell_timeout(Some(MIN_SHELL_TIMEOUT_MS)).is_ok());
        assert!(validate_shell_timeout(Some(MAX_SHELL_TIMEOUT_MS)).is_ok());
        assert!(validate_shell_timeout(Some(0)).is_err());
        assert!(validate_shell_timeout(Some(-1)).is_err());
        assert!(validate_shell_timeout(Some(MAX_SHELL_TIMEOUT_MS + 1)).is_err());
    }

    #[tokio::test]
    async fn run_shell_command_captures_exit_code_and_output() {
        let outcome = run_shell_command("echo shell-cron-ok", Path::new("."), MIN_SHELL_TIMEOUT_MS).await;
        assert!(outcome.succeeded(), "outcome: {outcome:?}");
        assert_eq!(outcome.exit_code, Some(0));
        assert!(!outcome.timed_out);
        assert!(outcome.stdout_tail.contains("shell-cron-ok"), "outcome: {outcome:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_command_reports_failure_with_stderr() {
        let outcome = run_shell_command("echo boom >&2; exit 3", Path::new("."), MIN_SHELL_TIMEOUT_MS).await;
        assert!(!outcome.succeeded());
        assert_eq!(outcome.exit_code, Some(3));
        assert!(outcome.stderr_tail.contains("boom"), "outcome: {outcome:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_command_kills_on_timeout() {
        let started = Instant::now();
        let outcome = run_shell_command("sleep 30", Path::new("."), MIN_SHELL_TIMEOUT_MS).await;
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(outcome.duration_ms < 10_000, "timeout should not wait 30s");
        assert!(started.elapsed().as_secs() < 15);
    }

    #[test]
    fn overflow_keeps_tail_and_counts_dropped() {
        let mut tail: Vec<u8> = Vec::new();
        let dropped = overflow(&mut tail, &[b'a'; 100]);
        assert_eq!(dropped, 0);
        assert_eq!(tail.len(), 100);

        let dropped = overflow(&mut tail, &[b'b'; MAX_CAPTURED_BYTES]);
        assert_eq!(dropped, 100);
        assert_eq!(tail.len(), MAX_CAPTURED_BYTES);
        assert_eq!(tail[MAX_CAPTURED_BYTES - 1], b'b');
    }

    #[test]
    fn decode_tail_marks_truncation() {
        let text = decode_tail(b"ok", 0);
        assert_eq!(text, "ok");

        let long = "x".repeat(MAX_REPORTED_CHARS + 10);
        let text = decode_tail(long.as_bytes(), 0);
        assert!(text.starts_with("…(earlier output truncated)…"));
        assert!(text.ends_with(&"x".repeat(MAX_REPORTED_CHARS)));
    }

    #[test]
    fn build_result_message_variants() {
        let command = "gh repo fork owner/repo --clone";

        let success = ShellOutcome {
            exit_code: Some(0),
            timed_out: false,
            stdout_tail: "Cloned into 'repo'".into(),
            stderr_tail: String::new(),
            duration_ms: 2_500,
        };
        let (message, tip_type) = build_result_message(command, &success);
        assert_eq!(tip_type, "success");
        assert!(message.contains("completed in 2s"));
        assert!(message.contains("Cloned into 'repo'"));
        assert!(!message.contains("stderr"));

        let failure = ShellOutcome {
            exit_code: Some(128),
            timed_out: false,
            stdout_tail: String::new(),
            stderr_tail: "fatal: not a git repository".into(),
            duration_ms: 1_200,
        };
        let (message, tip_type) = build_result_message(command, &failure);
        assert_eq!(tip_type, "error");
        assert!(message.contains("exit code 128"));
        assert!(message.contains("fatal: not a git repository"));

        let timeout = ShellOutcome {
            exit_code: None,
            timed_out: true,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            duration_ms: 600_000,
        };
        let (message, tip_type) = build_result_message(command, &timeout);
        assert_eq!(tip_type, "error");
        assert!(message.contains("timed out"));
    }
}
