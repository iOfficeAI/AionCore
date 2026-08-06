//! Client-hosted terminal service backing the ACP `terminal/*` methods.
//!
//! When the initialize handshake advertises `clientCapabilities.terminal`,
//! an agent may delegate command execution to us: `terminal/create` spawns
//! the process HERE (via the runtime spawn Builder), the agent polls
//! `terminal/output` / awaits `terminal/wait_for_exit`, and either side can
//! `terminal/kill`. We own the process and the output buffer, so the UI can
//! stream output live and the user can stop a single command without
//! killing the agent.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{Mutex, watch};
use tracing::{info, warn};

/// Default retained-output cap when the agent does not send
/// `outputByteLimit` (protocol: truncate from the FRONT, at char boundary).
pub const DEFAULT_OUTPUT_BYTE_LIMIT: u64 = 1024 * 1024;

/// Terminal command exit outcome (mirrors ACP `TerminalExitStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalExit {
    pub exit_code: Option<u32>,
    /// Present when terminated by signal (e.g. user stop → SIGKILL).
    pub signaled: bool,
}

/// Snapshot returned by [`TerminalRegistry::output`].
#[derive(Debug, Clone)]
pub struct TerminalOutputSnapshot {
    pub output: String,
    pub truncated: bool,
    pub exit: Option<TerminalExit>,
}

struct TerminalEntry {
    /// Held for kill; `None` once reaped (by the poller or by a kill).
    child: Arc<Mutex<Option<Child>>>,
    buffer: Arc<Mutex<OutputBuffer>>,
    exit_rx: watch::Receiver<Option<TerminalExit>>,
    /// Kills publish the exit themselves — the reaper may be gone by then.
    exit_tx: watch::Sender<Option<TerminalExit>>,
    command_line: String,
}

struct OutputBuffer {
    data: String,
    truncated: bool,
    limit: usize,
}

impl OutputBuffer {
    fn push(&mut self, chunk: &str) {
        self.data.push_str(chunk);
        if self.data.len() > self.limit {
            // Truncate from the beginning at a char boundary (protocol MUST).
            let mut cut = self.data.len() - self.limit;
            while !self.data.is_char_boundary(cut) {
                cut += 1;
            }
            self.data.drain(..cut);
            self.truncated = true;
        }
    }
}

/// Terminal ids are minted from a PROCESS-global counter, not a per-registry
/// one: an agent restart builds a fresh registry, and a per-registry counter
/// would hand out `aionui-term-1` again — colliding with the card the
/// frontend still has on screen for the same conversation (cards are keyed by
/// terminal id).
static NEXT_TERMINAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// All live terminals of ONE agent connection. Dropped/`kill_all`-ed with it.
#[derive(Default)]
pub struct TerminalRegistry {
    entries: Mutex<HashMap<String, TerminalEntry>>,
    /// Log correlation label (the owning conversation id; empty for probes).
    label: String,
    /// Fallback working directory when the agent sends no `cwd` — the
    /// conversation's workspace. Without it a command would inherit the
    /// aioncore process's cwd (the app bundle), which is never what the
    /// agent means.
    default_cwd: Option<std::path::PathBuf>,
}

impl TerminalRegistry {
    pub fn new(label: impl Into<String>, default_cwd: Option<std::path::PathBuf>) -> Self {
        Self {
            label: label.into(),
            default_cwd,
            ..Self::default()
        }
    }
}

/// Parameters for [`TerminalRegistry::create`] (decoded from
/// `terminal/create`).
pub struct CreateTerminalParams {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<std::path::PathBuf>,
    pub output_byte_limit: Option<u64>,
}

impl TerminalRegistry {
    /// Spawn the command and register a terminal for it.
    pub async fn create(&self, params: CreateTerminalParams) -> Result<String, String> {
        // Agents disagree on the `command` contract: grok wraps itself in
        // `/bin/bash -lc '<line>'` (command + args), while codebuddy sends the
        // whole compound line — `sleep 2 && echo hi` — as a bare `command`
        // with no args, expecting shell interpretation (live-captured
        // 2026-08-05: direct exec ENOENTs on the compound string). Rule:
        // explicit args → exec verbatim; no args → run the line through the
        // platform shell.
        let mut builder = if params.args.is_empty() {
            #[cfg(unix)]
            {
                let mut b = aionui_runtime::Builder::new("/bin/sh");
                b.arg("-c").arg(&params.command);
                b
            }
            #[cfg(windows)]
            {
                let mut b = aionui_runtime::Builder::new("cmd");
                b.arg("/C").arg(&params.command);
                b
            }
        } else {
            let mut b = aionui_runtime::Builder::new(&params.command);
            b.args(&params.args);
            b
        };
        builder
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in &params.env {
            builder.env(k, v);
        }
        if let Some(cwd) = params.cwd.as_ref().or(self.default_cwd.as_ref()) {
            builder.current_dir(cwd);
        }
        let mut child = builder.spawn().map_err(|e| format!("spawn failed: {e}"))?;

        let id = format!("aionui-term-{}", NEXT_TERMINAL_SEQ.fetch_add(1, Ordering::Relaxed) + 1);
        let command_line = std::iter::once(params.command.as_str())
            .chain(params.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        info!(
            conversation_id = %self.label,
            terminal_id = %id,
            command = %command_line,
            "client terminal created"
        );

        let limit = params.output_byte_limit.unwrap_or(DEFAULT_OUTPUT_BYTE_LIMIT) as usize;
        let buffer = Arc::new(Mutex::new(OutputBuffer {
            data: String::new(),
            truncated: false,
            limit: limit.max(1),
        }));
        let (exit_tx, exit_rx) = watch::channel(None);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(Some(child)));

        // Reader task: drain both streams into the ring buffer. It may OUTLIVE
        // the command — a backgrounded grandchild inherits the pipes and holds
        // them open — so exit detection must not depend on it.
        let task_buffer = Arc::clone(&buffer);
        tokio::spawn(async move {
            let mut out = stdout;
            let mut err = stderr;
            let mut out_buf = [0u8; 8192];
            let mut err_buf = [0u8; 8192];
            let mut out_open = out.is_some();
            let mut err_open = err.is_some();
            while out_open || err_open {
                tokio::select! {
                    r = async { out.as_mut().unwrap().read(&mut out_buf).await }, if out_open => {
                        match r {
                            Ok(0) | Err(_) => out_open = false,
                            Ok(n) => task_buffer.lock().await.push(&String::from_utf8_lossy(&out_buf[..n])),
                        }
                    }
                    r = async { err.as_mut().unwrap().read(&mut err_buf).await }, if err_open => {
                        match r {
                            Ok(0) | Err(_) => err_open = false,
                            Ok(n) => task_buffer.lock().await.push(&String::from_utf8_lossy(&err_buf[..n])),
                        }
                    }
                }
            }
        });

        // Reaper: poll the DIRECT child. `sh -c 'sleep 30 &'` exits instantly
        // while its backgrounded child keeps the stdout pipe open, so waiting
        // on stream EOF would hang the terminal forever (live-reproduced).
        // try_wait is polled rather than awaiting `wait()` under the lock so a
        // concurrent kill can always take the mutex.
        let reap_child = Arc::clone(&child);
        let reap_tx = exit_tx.clone();
        let reap_id = id.clone();
        tokio::spawn(async move {
            loop {
                {
                    let mut guard = reap_child.lock().await;
                    match guard.as_mut() {
                        Some(c) => match c.try_wait() {
                            Ok(Some(status)) => {
                                *guard = None;
                                #[cfg(unix)]
                                let signaled = {
                                    use std::os::unix::process::ExitStatusExt as _;
                                    status.signal().is_some()
                                };
                                #[cfg(not(unix))]
                                let signaled = false;
                                let _ = reap_tx.send(Some(TerminalExit {
                                    exit_code: status.code().map(|c| c as u32),
                                    signaled,
                                }));
                                return;
                            }
                            Ok(None) => {}
                            Err(e) => {
                                warn!(terminal_id = %reap_id, error = %e, "terminal wait failed");
                                *guard = None;
                                let _ = reap_tx.send(Some(TerminalExit {
                                    exit_code: None,
                                    signaled: false,
                                }));
                                return;
                            }
                        },
                        // A kill (or release) already reaped and published.
                        None => return,
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });

        self.entries.lock().await.insert(
            id.clone(),
            TerminalEntry {
                child,
                buffer,
                exit_rx,
                exit_tx,
                command_line,
            },
        );
        Ok(id)
    }

    /// Current output snapshot (protocol `terminal/output`).
    pub async fn output(&self, terminal_id: &str) -> Option<TerminalOutputSnapshot> {
        let entries = self.entries.lock().await;
        let entry = entries.get(terminal_id)?;
        let buf = entry.buffer.lock().await;
        Some(TerminalOutputSnapshot {
            output: buf.data.clone(),
            truncated: buf.truncated,
            exit: *entry.exit_rx.borrow(),
        })
    }

    /// The command line a terminal is running (for UI display).
    pub async fn command_line(&self, terminal_id: &str) -> Option<String> {
        let entries = self.entries.lock().await;
        Some(entries.get(terminal_id)?.command_line.clone())
    }

    /// Await command completion (protocol `terminal/wait_for_exit`).
    pub async fn wait_for_exit(&self, terminal_id: &str) -> Option<TerminalExit> {
        let mut rx = {
            let entries = self.entries.lock().await;
            entries.get(terminal_id)?.exit_rx.clone()
        };
        loop {
            if let Some(exit) = *rx.borrow() {
                return Some(exit);
            }
            if rx.changed().await.is_err() {
                return (*rx.borrow()).or(Some(TerminalExit {
                    exit_code: None,
                    signaled: true,
                }));
            }
        }
    }

    /// Force-kill the command (protocol `terminal/kill`, also the user's
    /// stop button). The terminal stays registered so output/exit status
    /// remain queryable until `release`.
    pub async fn kill(&self, terminal_id: &str, source: &str) -> bool {
        let Some((child, exit_tx)) = ({
            let entries = self.entries.lock().await;
            entries
                .get(terminal_id)
                .map(|e| (Arc::clone(&e.child), e.exit_tx.clone()))
        }) else {
            return false;
        };
        let mut guard = child.lock().await;
        if let Some(c) = guard.as_mut() {
            info!(terminal_id, source, "client terminal killed");
            // kill_process_tree reaps the child itself, so publish the signal
            // exit here — the reaper sees `None` and stops without a status.
            let _ = aionui_runtime::kill_process_tree(c).await;
            *guard = None;
            let _ = exit_tx.send(Some(TerminalExit {
                exit_code: None,
                signaled: true,
            }));
        }
        true
    }

    /// Kill (if still running) and forget the terminal (protocol
    /// `terminal/release`).
    pub async fn release(&self, terminal_id: &str) -> bool {
        let entry = self.entries.lock().await.remove(terminal_id);
        match entry {
            Some(entry) => {
                let mut guard = entry.child.lock().await;
                if let Some(c) = guard.as_mut() {
                    let _ = aionui_runtime::kill_process_tree(c).await;
                    *guard = None;
                    let _ = entry.exit_tx.send(Some(TerminalExit {
                        exit_code: None,
                        signaled: true,
                    }));
                }
                true
            }
            None => false,
        }
    }

    /// Tear down every terminal (agent kill / conversation delete).
    pub async fn kill_all(&self) {
        let ids: Vec<String> = self.entries.lock().await.keys().cloned().collect();
        if !ids.is_empty() {
            info!(count = ids.len(), "tearing down all client terminals");
        }
        for id in ids {
            self.release(&id).await;
        }
    }

    /// Ids of all live (not yet released) terminals.
    pub async fn ids(&self) -> Vec<String> {
        self.entries.lock().await.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(cmd: &str, args: &[&str]) -> CreateTerminalParams {
        CreateTerminalParams {
            command: cmd.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        }
    }

    #[tokio::test]
    async fn create_runs_command_and_reports_output_and_exit() {
        let reg = TerminalRegistry::new("conv-t", None);
        let id = reg.create(params("echo", &["hello_term"])).await.unwrap();
        let exit = reg.wait_for_exit(&id).await.unwrap();
        assert_eq!(exit.exit_code, Some(0));
        assert!(!exit.signaled);
        let snap = reg.output(&id).await.unwrap();
        assert!(snap.output.contains("hello_term"));
        assert!(!snap.truncated);
        assert!(snap.exit.is_some());
    }

    #[tokio::test]
    async fn output_byte_limit_truncates_from_front() {
        let reg = TerminalRegistry::new("conv-t", None);
        let mut p = params("sh", &["-c", "printf 'AAAA'; printf 'BBBB'"]);
        p.output_byte_limit = Some(4);
        let id = reg.create(p).await.unwrap();
        reg.wait_for_exit(&id).await.unwrap();
        let snap = reg.output(&id).await.unwrap();
        assert_eq!(snap.output, "BBBB");
        assert!(snap.truncated);
    }

    #[tokio::test]
    async fn kill_terminates_long_command_with_signal() {
        let reg = TerminalRegistry::new("conv-t", None);
        let id = reg.create(params("sleep", &["30"])).await.unwrap();
        assert!(reg.kill(&id, "test").await);
        let exit = reg.wait_for_exit(&id).await.unwrap();
        assert!(exit.signaled || exit.exit_code.is_none() || exit.exit_code != Some(0));
        // Still queryable until release.
        assert!(reg.output(&id).await.is_some());
        assert!(reg.release(&id).await);
        assert!(reg.output(&id).await.is_none());
    }

    #[tokio::test]
    async fn release_is_idempotent_and_kill_all_clears() {
        let reg = TerminalRegistry::new("conv-t", None);
        let id1 = reg.create(params("sleep", &["30"])).await.unwrap();
        let _id2 = reg.create(params("sleep", &["30"])).await.unwrap();
        assert!(reg.release(&id1).await);
        assert!(!reg.release(&id1).await);
        reg.kill_all().await;
        assert!(reg.ids().await.is_empty());
    }

    #[tokio::test]
    async fn unknown_terminal_id_is_none_or_false() {
        let reg = TerminalRegistry::new("conv-t", None);
        assert!(reg.output("nope").await.is_none());
        assert!(reg.wait_for_exit("nope").await.is_none());
        assert!(!reg.kill("nope", "test").await);
        assert!(!reg.release("nope").await);
    }

    #[tokio::test]
    async fn bare_compound_command_runs_through_shell() {
        let reg = TerminalRegistry::new("conv-t", None);
        let mut p = params("true && echo shell_interpreted", &[]);
        p.args = vec![];
        let id = reg.create(p).await.unwrap();
        let exit = reg.wait_for_exit(&id).await.unwrap();
        assert_eq!(exit.exit_code, Some(0));
        let snap = reg.output(&id).await.unwrap();
        assert!(snap.output.contains("shell_interpreted"), "got: {}", snap.output);
    }

    #[tokio::test]
    async fn background_child_holding_stdout_does_not_hang_exit() {
        // B6: `sh -c 'sleep 30 &'` — the parent exits immediately but the
        // backgrounded child inherits the stdout pipe, so the read side never
        // sees EOF. Waiting on stream close alone would hang forever.
        let reg = TerminalRegistry::new("conv-t", None);
        let id = reg
            .create(params("sh", &["-c", "sleep 30 & echo parent_done"]))
            .await
            .unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), reg.wait_for_exit(&id))
            .await
            .expect("exit must be reported once the direct child exits, not when the pipe closes")
            .unwrap();
        assert_eq!(exit.exit_code, Some(0));
        let snap = reg.output(&id).await.unwrap();
        assert!(snap.output.contains("parent_done"), "got: {}", snap.output);
    }

    #[tokio::test]
    async fn default_cwd_applies_when_agent_sends_none() {
        let dir = std::env::temp_dir().join("aionui-term-cwd-test");
        std::fs::create_dir_all(&dir).unwrap();
        let reg = TerminalRegistry::new("conv-t", Some(dir.clone()));
        let id = reg.create(params("pwd", &[])).await.unwrap();
        reg.wait_for_exit(&id).await.unwrap();
        let snap = reg.output(&id).await.unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        assert!(
            snap.output
                .trim()
                .ends_with(canonical.file_name().unwrap().to_str().unwrap()),
            "expected cwd {} in output: {}",
            canonical.display(),
            snap.output
        );
    }

    #[tokio::test]
    async fn terminal_ids_are_unique_across_registries() {
        // A rebuilt agent gets a fresh registry; ids must not restart at 1 or
        // the frontend's terminal-id-keyed card collides with a stale one.
        let a = TerminalRegistry::new("conv-a", None);
        let b = TerminalRegistry::new("conv-b", None);
        let id_a = a.create(params("true", &[])).await.unwrap();
        let id_b = b.create(params("true", &[])).await.unwrap();
        assert_ne!(id_a, id_b);
    }
}
