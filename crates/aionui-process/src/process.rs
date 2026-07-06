//! `ManagedProcess` — the live handle for a subprocess THIS crate spawned.
//!
//! Freshly written (not moved from the existing CliAgentProcess). Owns the
//! child's raw stdio for byte-duplex handoff (IC: bytes not semantics — it
//! never parses output), a watch latch that flips to the exit status, a
//! bounded stderr ring buffer for diagnostics, and the background tasks that
//! drain stderr + monitor exit. Reaping is EXCLUSIVELY via tokio's per-Child
//! wait() (IC-2: no waitpid(-1) / no SIGCHLD handler / no raw Command).

use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;

use aionui_common::CommandSpec;
use aionui_runtime::Builder;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::ProcessError;

/// Max bytes retained in the stderr ring buffer (diagnostics only).
const STDERR_BUFFER_MAX: usize = 8192;

/// Boxed stdio handed to a transport (type erased so the handle type — real
/// ChildStdin/out vs a fake duplex — is hidden).
pub type BoxedStdin = Box<dyn tokio::io::AsyncWrite + Send + Unpin + 'static>;
pub type BoxedStdout = Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>;

/// Terminal exit state held in the exit watch. `None` in the watch = STILL
/// RUNNING; `Some(_)` = terminally gone. The two `Some` variants distinguish a
/// real status from a `wait()` that errored — the distinction the old
/// `Option<ExitStatus>` payload could not express (F45): on a `wait()` error the
/// old code left the payload `None`, making a terminally-gone process look like
/// "still running" to `exit_status()`/`kill()` and causing a spurious 5s
/// "did not exit" timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalExit {
    /// `wait()` returned a status (the normal case).
    Exited(ExitStatus),
    /// `wait()` itself errored (e.g. ECHILD — another reaper got there first).
    /// The process is terminally gone but with no obtainable status.
    WaitErrored,
}

/// A registry-deregistration (or other cleanup) hook fired exactly once when the
/// `ManagedProcess` is dropped. Opaque to this crate ("bytes not semantics" — it
/// never knows about RegistryStore); injected by the assembly layer (RealSpawner)
/// so a clean turn-end drop removes the registry row (F38) without this module
/// depending on the registry. The hook MUST NOT block the calling thread — the
/// injector offloads any file I/O (e.g. to `spawn_blocking`).
type OnDropHook = Box<dyn FnOnce() + Send>;

/// A live subprocess this crate owns.
pub struct ManagedProcess {
    stdin: Mutex<Option<ChildStdin>>,
    stdout: Mutex<Option<ChildStdout>>,
    pid: u32,
    process_group_id: Option<u32>,
    exit_rx: watch::Receiver<Option<TerminalExit>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    /// The exit-monitor task. Held only to keep it attached for the process's
    /// lifetime; on Drop it is DETACHED (not aborted) so it completes the single
    /// `child.wait()` reap of the direct child after the group-kill — one reaper,
    /// no manual `waitpid`, no blocking sleep (F43/F44). `Option` so Drop can
    /// `take()` it and explicitly detach (documents intent + silences "unread").
    exit_task: Option<JoinHandle<()>>,
    /// Fired once on Drop (registry dereg, F38). `Mutex` so `&self` Drop can take it.
    on_drop: std::sync::Mutex<Option<OnDropHook>>,
    /// Windows Job Object holding this child + all its descendants (batch B).
    /// Held for the child's whole lifetime; on Drop it terminates the whole
    /// subtree (KILL_ON_JOB_CLOSE). `None` only if assignment failed (the spawn
    /// then errors before this struct is built, so in practice always `Some`).
    #[cfg(windows)]
    job: Option<job_windows::JobObject>,
}

impl Drop for ManagedProcess {
    /// Owning-handle contract: dropping a `ManagedProcess` tears down the
    /// subprocess subtree it owns — WITHOUT blocking the calling thread (which,
    /// in the consumer, is a tokio worker; F43) and WITHOUT a second reaper
    /// racing the exit task (F44).
    ///
    /// Steps: (1) group-SIGKILL the whole subtree if not already terminally gone
    /// (covers grandchildren in the group; `force_kill` maps already-gone→Ok so a
    /// normal post-exit drop is a no-op); (2) abort the stderr task; (3) detach
    /// the exit-monitor task by dropping its handle; (4) fire the once-only
    /// on-drop hook (registry dereg, F38).
    ///
    /// WHO REAPS THE DIRECT CHILD'S ZOMBIE (corrected per review):
    /// the authoritative reaper is tokio's `kill_on_drop(true)` (set on the
    /// Builder Child in aionui-runtime spawn.rs) — when the exit-monitor task
    /// (which OWNS the `Child`) is dropped, tokio's orphan queue reaps the Child.
    /// So we do NOT rely on the detached `child.wait()` running to completion
    /// (a detached task is cancelled at its next await on runtime shutdown, NOT
    /// driven to completion — the earlier claim here was overstated). We detach
    /// rather than `abort()` purely so a turn-end drop (runtime still alive) lets
    /// the in-flight `wait()` finish naturally; either way kill_on_drop guarantees
    /// the reap. The point that IS load-bearing (F43/F44): Drop does NO blocking
    /// `std::thread::sleep` and NO manual `waitpid`, so it never stalls the
    /// (tokio worker) thread releasing the last Arc and there is no second reaper
    /// racing tokio's.
    ///
    /// `already_terminal` now reads the watch as a real liveness check (F45):
    /// `Some(_)` — whether `Exited` or `WaitErrored` — means terminally gone, so
    /// a `wait()`-error no longer makes a dead process look alive and trigger a
    /// redundant group-kill.
    fn drop(&mut self) {
        let already_terminal = self.exit_rx.borrow().is_some();
        if !already_terminal {
            // HOT-path whole-subtree kill. Non-blocking.
            //
            // F41 (why no identity re-gate on the HOT path, unlike cold reap):
            // the exit-monitor task still holds the tokio `Child`, which OWNS
            // this pid — the kernel cannot recycle a pid until it is
            // `wait()`-reaped, and that reap only happens inside exit_task (which
            // we detach, not abort). So while this handle is live the pgid/job
            // CANNOT have been recycled onto an innocent process; killing our own
            // recorded group/job here is safe by Child-ownership, no start-time
            // re-check needed. The identity gate is required only for the COLD
            // reap path (no live Child, pid reconstructed from disk), where it
            // exists (F42). `already_terminal` uses the F45-correct watch so a
            // wait()-errored (terminally gone) process is not re-killed.
            #[cfg(unix)]
            let _ = crate::force_kill(self.pid, self.process_group_id);
            // Windows HOT kill = terminate the whole Job (subtree), not just the
            // single process. The Job's own Drop (below, KILL_ON_JOB_CLOSE) would
            // also do this, but an explicit synchronous terminate here guarantees
            // teardown before the dereg hook fires.
            #[cfg(windows)]
            if let Some(job) = &self.job {
                job.terminate();
            }
        }
        // Stop draining stderr eagerly (its fd/buffer/task would otherwise be
        // pinned by a surviving grandchild holding the write end; F48).
        self.stderr_task.abort();
        // DETACH the exit task (do NOT abort): take it out and drop it. Dropping
        // a tokio `JoinHandle` lets the task run to completion, so the single
        // `child.wait()` inside it completes the lone reap of the direct child
        // after the group-kill above — one reaper, no manual `waitpid`, no
        // `std::thread::sleep`, no blocking on this (worker) thread (F43/F44).
        if let Some(task) = self.exit_task.take() {
            drop(task); // explicit detach
        }

        // Fire the dereg hook exactly once (registry dereg, F38). The injector
        // (RealSpawner) makes the hook non-blocking (offloads file I/O), so this
        // does not stall the dropping thread.
        if let Some(hook) = self.on_drop.lock().unwrap_or_else(|e| e.into_inner()).take() {
            hook();
        }
    }
}

impl ManagedProcess {
    /// Spawn `spec` as its own process-group leader and start the background
    /// stderr-drain + exit-monitor tasks. `extra_env` is applied per-child via
    /// the Builder (IC-5: never mutates global env).
    pub async fn spawn(spec: CommandSpec, extra_env: &[(String, String)]) -> Result<Self, ProcessError> {
        let cwd = match spec.cwd.as_deref() {
            Some(c) => Some(prepare_command_cwd(c)?),
            None => None,
        };

        let mut builder = Builder::new(&spec.command);
        builder
            .args(&spec.args)
            .envs(spec.env.iter().map(|e| (&e.name, &e.value)))
            .envs(extra_env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = cwd {
            builder.current_dir(cwd);
        }

        let mut child = builder
            .spawn()
            .map_err(|e| ProcessError::internal(format!("spawn '{}' failed: {e}", spec.command.display())))?;

        let pid = child
            .id()
            .ok_or_else(|| ProcessError::internal("spawned child has no pid"))?;
        let process_group_id = tracked_process_group_id(pid);

        // Windows (batch B): the child was spawned CREATE_SUSPENDED. Assign it to
        // a Job Object (subtree containment) and resume it BEFORE we touch its
        // stdio — closing the assign-vs-fork race. On failure we must NOT leave a
        // suspended zombie (S7): kill it and surface the error.
        #[cfg(windows)]
        let job = {
            // raw_handle() is valid while the child is alive (it is, suspended).
            match child.raw_handle() {
                Some(raw) => match job_windows::JobObject::assign_and_resume(raw, pid) {
                    Ok(j) => Some(j),
                    Err(e) => {
                        // Tear down the suspended child (kill_on_drop SIGKILLs it
                        // when `child` drops at end of scope) and error out (S7).
                        let _ = child.start_kill();
                        return Err(ProcessError::internal(format!("windows job containment: {e}")));
                    }
                },
                None => {
                    let _ = child.start_kill();
                    return Err(ProcessError::internal(
                        "windows: child has no raw handle for job assignment",
                    ));
                }
            }
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProcessError::internal("failed to capture stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessError::internal("failed to capture stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessError::internal("failed to capture stderr"))?;

        // Background: drain stderr into a bounded ring buffer.
        let stderr_buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let buf_clone = Arc::clone(&stderr_buffer);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(pid, stderr = trimmed, "subprocess stderr");
                }
                let mut buf = buf_clone.lock().await;
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                if buf.len() > STDERR_BUFFER_MAX {
                    // Truncate by raw byte offset (UTF-8-safe: we never index
                    // mid-char, we drain a byte prefix of a Vec<u8>).
                    let cut = buf.len() - STDERR_BUFFER_MAX;
                    buf.drain(..cut);
                }
            }
            debug!(pid, "stderr drain finished");
        });

        // Background: monitor exit via tokio's per-Child wait (IC-2). The watch
        // payload is `Option<TerminalExit>`: `None` = still running, `Some(_)` =
        // terminally gone. A `wait()` error latches `Some(WaitErrored)` — NOT
        // `None` — so `exit_status()`/`kill()` see a real terminal state instead
        // of conflating "wait errored" with "never exited" (F45).
        let (exit_tx, exit_rx) = watch::channel(None);
        // Hand the exit task an abort handle to the stderr drain so it can stop
        // it the moment the direct child is gone — otherwise a grandchild holding
        // the stderr write end keeps the drain (fd + 8KB buffer + task) pinned
        // until the ManagedProcess is dropped (F48).
        let stderr_abort = stderr_task.abort_handle();
        let exit_task = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    debug!(pid, ?status, "subprocess exited");
                    let _ = exit_tx.send(Some(TerminalExit::Exited(status)));
                }
                Err(e) => {
                    error!(pid, error = %e, "wait() on subprocess errored");
                    let _ = exit_tx.send(Some(TerminalExit::WaitErrored));
                }
            }
            // Direct child is gone: stop the stderr drain so a surviving
            // grandchild on the same pipe cannot pin the fd/task (F48).
            stderr_abort.abort();
        });

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(stdout)),
            pid,
            process_group_id,
            exit_rx,
            stderr_buffer,
            stderr_task,
            exit_task: Some(exit_task),
            on_drop: std::sync::Mutex::new(None),
            #[cfg(windows)]
            job,
        })
    }

    /// Install a once-only cleanup hook fired on Drop (registry dereg, F38).
    /// Opaque to this crate; the injector (RealSpawner) must make it non-blocking.
    /// Overwrites any previously-set hook (last writer wins; intended single set).
    pub fn set_on_drop(&self, hook: Box<dyn FnOnce() + Send>) {
        *self.on_drop.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    /// Hand off stdin+stdout (boxed) to a transport. Once-only: a second call
    /// returns None. Takes both under one critical section so it is all-or-
    /// nothing — a partial handoff never leaves one taken and one intact.
    pub async fn take_stdio(&self) -> Option<(BoxedStdin, BoxedStdout)> {
        let mut sin = self.stdin.lock().await;
        let mut sout = self.stdout.lock().await;
        match (sin.is_some(), sout.is_some()) {
            (true, true) => {
                let i = sin.take().unwrap();
                let o = sout.take().unwrap();
                Some((Box::new(i), Box::new(o)))
            }
            _ => None, // already taken (or never present) — leave both as-is
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    /// The exit status if the process has terminally exited WITH an obtainable
    /// status. `None` means EITHER still running OR `wait()` errored
    /// (`WaitErrored`) — callers needing to distinguish those use
    /// [`Self::terminal_exit`] / [`Self::has_exited`].
    pub fn exit_status(&self) -> Option<ExitStatus> {
        match *self.exit_rx.borrow() {
            Some(TerminalExit::Exited(s)) => Some(s),
            Some(TerminalExit::WaitErrored) | None => None,
        }
    }

    /// The full terminal state: `None` = still running; `Some(Exited)` /
    /// `Some(WaitErrored)` = terminally gone. This is the F45-correct liveness
    /// signal (a `WaitErrored` process is GONE, not running).
    pub fn terminal_exit(&self) -> Option<TerminalExit> {
        *self.exit_rx.borrow()
    }

    /// True iff the process is terminally gone (exited or wait-errored).
    pub fn has_exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }

    /// Resolve when the process is terminally gone (exited or wait() errored).
    /// Returns the status if one was obtainable (`None` for `WaitErrored`).
    pub async fn wait_for_exit(&self) -> Option<ExitStatus> {
        let mut rx = self.exit_rx.clone();
        if rx.borrow().is_some() {
            return self.exit_status();
        }
        let _ = rx.changed().await;
        self.exit_status()
    }

    /// Peek the last `max_lines` lines of buffered stderr without draining.
    pub async fn peek_stderr_tail(&self, max_lines: usize) -> String {
        if max_lines == 0 {
            return String::new();
        }
        let buf = self.stderr_buffer.lock().await;
        // Lossy render — diagnostics channel, never the byte-duplex contract.
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim_end_matches('\n');
        if trimmed.is_empty() {
            return String::new();
        }
        let mut tail: Vec<&str> = trimmed.rsplit('\n').take(max_lines).collect();
        tail.reverse();
        tail.join("\n")
    }

    /// Close stdin (signals EOF to the child) without killing.
    pub async fn close_stdin(&self) {
        if self.stdin.lock().await.take().is_some() {
            debug!(pid = self.pid, "stdin closed");
        }
    }

    /// Non-destructive interrupt: close stdin so a cooperative child sees EOF
    /// and winds down on its own. Distinct from kill (which force-terminates).
    pub async fn interrupt(&self) -> Result<(), ProcessError> {
        self.close_stdin().await;
        Ok(())
    }

    /// Graceful kill: close stdin, wait up to `grace` for self-exit, then
    /// group-SIGKILL. Confirms exit before returning.
    pub async fn kill(&self, grace: Duration) -> Result<(), ProcessError> {
        self.close_stdin().await;

        let mut rx = self.exit_rx.clone();
        let exited = tokio::time::timeout(grace, async {
            if rx.borrow().is_some() {
                return;
            }
            let _ = rx.changed().await;
        })
        .await;
        if exited.is_ok() && self.exit_rx.borrow().is_some() {
            debug!(pid = self.pid, "exited within grace");
            return Ok(());
        }

        warn!(pid = self.pid, "grace expired, force-killing subtree");
        // Unix: group-SIGKILL. Windows: terminate the whole Job (subtree), not
        // the single process — the live handle holds the job (hot path).
        #[cfg(unix)]
        crate::force_kill(self.pid, self.process_group_id)?;
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        } else {
            crate::force_kill(self.pid, self.process_group_id)?;
        }

        // Wait for the exit monitor to observe termination so callers don't
        // race a still-live child after the kill returns.
        let mut rx = self.exit_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            if rx.borrow().is_some() {
                return;
            }
            let _ = rx.changed().await;
        })
        .await
        .map_err(|_| ProcessError::internal(format!("process {} did not exit after SIGKILL", self.pid)))?;
        Ok(())
    }
}

/// Validate a workspace cwd: non-empty, no whitespace segment (bundled runtime
/// can't handle it), exists, is a directory.
pub(crate) fn prepare_command_cwd(cwd: &str) -> Result<PathBuf, ProcessError> {
    if cwd.trim().is_empty() {
        return Err(ProcessError::bad_request("workspace directory is empty"));
    }
    let path = PathBuf::from(cwd);
    if path
        .components()
        .any(|c| c.as_os_str().to_string_lossy().contains(char::is_whitespace))
    {
        return Err(ProcessError::workspace_path_contains_whitespace_runtime_unsupported(
            path.display().to_string(),
        ));
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Ok(path),
        Ok(_) => Err(ProcessError::bad_request(format!(
            "workspace path is not a directory: {}",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ProcessError::bad_request(format!(
            "workspace directory does not exist: {}",
            path.display()
        ))),
        Err(e) => Err(ProcessError::bad_request(format!(
            "workspace directory not accessible: {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
pub(crate) fn tracked_process_group_id(pid: u32) -> Option<u32> {
    Some(pid) // Builder sets process_group(0): the leader's pgid == its pid.
}

#[cfg(not(unix))]
pub(crate) fn tracked_process_group_id(_pid: u32) -> Option<u32> {
    None
}

/// Windows Job Object subtree containment (feature 005 batch B). Transcribed
/// from watchexec/process-wrap's verified `src/windows.rs` scheme (Decision 2b:
/// borrow the SCHEME, not the crate). A `JobObject` owns a Job handle for the
/// child's whole lifetime: every descendant the child spawns is auto-trapped in
/// the job (Windows inherits job membership), so `terminate()` /
/// `KILL_ON_JOB_CLOSE` reaps the WHOLE subtree — the Windows analog of the Unix
/// process-group kill, and STRONGER (a `setsid`-style breakaway is not possible
/// without `JOB_OBJECT_LIMIT_BREAKAWAY_OK`).
#[cfg(windows)]
pub(crate) mod job_windows {
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// An owned Job Object handle. RAII: dropping it closes the handle, which —
    /// because we set `KILL_ON_JOB_CLOSE` — terminates every process still in the
    /// job (the Windows kill-on-drop guarantee, even if our supervisor crashes).
    pub(crate) struct JobObject {
        job: HANDLE,
    }

    // SAFETY: a Windows job HANDLE is just a kernel handle value; it is safe to
    // move/share across threads (we only ever close it once, on Drop).
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    impl JobObject {
        /// Create a job, set `KILL_ON_JOB_CLOSE`, assign the (suspended) child to
        /// it, then resume the child's threads. The child MUST have been spawned
        /// with `CREATE_SUSPENDED` (runtime's `configure_platform_spawn`) so it
        /// has not yet run any instruction — closing the assign-vs-fork race.
        ///
        /// `child_raw` is tokio's `Child::raw_handle()` — we use it read-only for
        /// `AssignProcessToJobObject` and NEVER close it (tokio owns it; a
        /// double-close would be a use-after-free). `pid` is used only to find
        /// the child's threads for resume (std/tokio expose no primary-thread
        /// handle, hence the toolhelp walk).
        pub(crate) fn assign_and_resume(child_raw: RawHandle, pid: u32) -> Result<Self, String> {
            // SAFETY: CreateJobObjectW with null attrs/name returns a job handle
            // or null on failure.
            let job = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
            if job.is_null() {
                return Err(format!("CreateJobObjectW failed: {}", std::io::Error::last_os_error()));
            }
            let this = JobObject { job };

            // Set KILL_ON_JOB_CLOSE so dropping the last job handle kills the
            // whole subtree (crash-safety).
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: info is a valid, fully-initialized struct of the given size.
            let ok = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    core::ptr::from_ref(&info).cast(),
                    core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                return Err(format!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            // Assign the suspended child to the job. Descendants inherit it.
            // SAFETY: child_raw is a live process handle owned by tokio; we only
            // read it here.
            let ok = unsafe { AssignProcessToJobObject(job, child_raw as HANDLE) };
            if ok == 0 {
                return Err(format!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }

            // Resume the child (it was spawned CREATE_SUSPENDED). std/tokio give
            // no primary-thread handle, so enumerate the system thread table and
            // resume every thread owned by our pid (process-wrap's documented
            // "terrible hack" — the only option without reimplementing spawn).
            resume_threads(pid).map_err(|e| format!("resume failed: {e}"))?;

            Ok(this)
        }

        /// Synchronously terminate every process in the job (the whole subtree).
        pub(crate) fn terminate(&self) {
            // SAFETY: self.job is a valid job handle for our lifetime.
            unsafe { TerminateJobObject(self.job, 1) };
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // Closing the last job handle triggers KILL_ON_JOB_CLOSE → the whole
            // subtree dies. This IS the Windows kill-on-drop guarantee.
            // SAFETY: we own the handle and close it exactly once.
            unsafe { CloseHandle(self.job) };
        }
    }

    /// Resume all threads of `pid` via a system-wide toolhelp thread snapshot.
    /// Edge cases (from process-wrap source): THREADENTRY32.dwSize MUST be the
    /// struct size or Thread32First silently returns nothing; ResumeThread's
    /// error sentinel is `u32::MAX` (not -1); the snapshot is SYSTEM-WIDE
    /// (TH32CS_SNAPTHREAD ignores the pid arg) so we filter by th32OwnerProcessID.
    fn resume_threads(pid: u32) -> Result<(), std::io::Error> {
        // SAFETY: returns a snapshot handle or INVALID_HANDLE_VALUE.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let result = (|| {
            let mut entry: THREADENTRY32 = unsafe { core::mem::zeroed() };
            entry.dwSize = core::mem::size_of::<THREADENTRY32>() as u32;
            // SAFETY: entry is a valid, dwSize-initialized THREADENTRY32.
            if unsafe { Thread32First(snap, &mut entry) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            loop {
                if entry.th32OwnerProcessID == pid {
                    // SAFETY: open the thread for suspend/resume by tid.
                    let th = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                    if !th.is_null() {
                        // SAFETY: th is a valid thread handle; resume + close it.
                        let rc = unsafe { ResumeThread(th) };
                        unsafe { CloseHandle(th) };
                        if rc == u32::MAX {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                }
                // SAFETY: entry stays valid; Thread32Next advances or signals end.
                if unsafe { Thread32Next(snap, &mut entry) } == 0 {
                    break; // no more threads
                }
            }
            Ok(())
        })();
        // SAFETY: close the snapshot handle we created.
        unsafe { CloseHandle(snap) };
        result
    }
}
