//! Internal one-shot client. No route, identity, permission or handler-file resolution.
//!
//! Only an opt-in Linux Core can invoke its own private helper. The owning task
//! reaps the direct child before returning, including after cancellation or a
//! dropped caller. This requires the Tokio runtime to remain alive; host death
//! and runtime shutdown require separate lifecycle handling. This is not an OS sandbox.
use serde_json::Value;
use tokio::sync::oneshot;

/// Fixed protocol/process failures; plugin exception objects are never formatted.
#[derive(Debug, PartialEq, Eq)]
pub enum HelperError {
    Unavailable,
    InputLimit,
    Cancelled,
    Timeout,
    Spawn,
    Io,
    OutputLimit,
    Protocol,
    ChildExit,
    Cleanup,
    OwnerStopped,
    ParentGuard,
    Execution(String),
}

/// Fixed host-owned metadata, not an authorization token or plugin input.
pub const PARENT_PID_ENV: &str = "WORKMATE_EXTENSION_JS_PARENT_PID";

/// Arm Linux parent-death protection ONLY in the private helper's early entry,
/// before reading stdin or evaluating JS. This changes this process's signal
/// lifecycle; never call it in the hosting Core server. Missing/invalid metadata
/// or a failed kernel operation rejects execution, rather than weakening it.
pub fn prepare_child_parent_guard() -> Result<(), &'static str> {
    #[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
    {
        enabled::prepare_child_parent_guard()
    }
    #[cfg(not(all(feature = "experimental-extension-js", target_os = "linux")))]
    {
        Err("runner_unavailable")
    }
}

/// Compute from already validated source/arguments, never from a client-selected
/// executable or path. Dropping the cancellation sender also cancels the call.
/// Dropping this future notifies the independent process owner to kill and wait.
/// Callers must still establish user, curated-source and plan-safe authorization.
pub async fn invoke(source: String, input: Value, cancel: oneshot::Receiver<()>) -> Result<Value, HelperError> {
    #[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
    {
        enabled::invoke(source, input, cancel).await
    }
    #[cfg(not(all(feature = "experimental-extension-js", target_os = "linux")))]
    {
        let _ = (source, input, cancel);
        Err(HelperError::Unavailable)
    }
}

#[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
mod enabled {
    use super::*;
    use aionui_runtime::{Builder, kill_process_tree};
    use serde::{Deserialize, Serialize};
    use std::io::{self, Write};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
    use tokio::process::Child;
    use tokio::time::Instant;

    const JSON_MAX: usize = 64 * 1024;
    const REQUEST_MAX: usize = 256 * 1024;
    const STDOUT_MAX: usize = 256 * 1024;
    const STDERR_MAX: usize = 8 * 1024;
    const CALL_BUDGET: Duration = Duration::from_secs(2);

    struct LimitedBuffer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for LimitedBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(io::Error::other("JSON limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, HelperError> {
        let mut buffer = LimitedBuffer {
            bytes: Vec::new(),
            limit,
        };
        serde_json::to_writer(&mut buffer, value).map_err(|_| HelperError::InputLimit)?;
        Ok(buffer.bytes)
    }
    fn request(source: &str, input: &Value) -> Result<Vec<u8>, HelperError> {
        if source.len() > JSON_MAX {
            return Err(HelperError::InputLimit);
        }
        // Bound traversal/serializer recursion, even for a Value assembled in Rust.
        let mut pending = vec![(input, 0usize)];
        let mut nodes = 0usize;
        while let Some((value, depth)) = pending.pop() {
            nodes += 1;
            if depth > 64 || nodes + pending.len() > 32 * 1024 {
                return Err(HelperError::InputLimit);
            }
            let children = match value {
                Value::Array(items) => items.len(),
                Value::Object(items) => items.len(),
                _ => 0,
            };
            if nodes + pending.len() + children > 32 * 1024 {
                return Err(HelperError::InputLimit);
            }
            match value {
                Value::Array(items) => pending.extend(items.iter().map(|v| (v, depth + 1))),
                Value::Object(items) => pending.extend(items.values().map(|v| (v, depth + 1))),
                _ => {}
            }
        }
        encode(input, JSON_MAX)?;
        #[derive(Serialize)]
        struct Request<'a> {
            source: &'a str,
            input: &'a Value,
        }
        encode(&Request { source, input }, REQUEST_MAX)
    }
    fn cancelled(receiver: &mut oneshot::Receiver<()>) -> bool {
        !matches!(receiver.try_recv(), Err(oneshot::error::TryRecvError::Empty))
    }
    pub(super) fn prepare_child_parent_guard() -> Result<(), &'static str> {
        let expected: libc::pid_t = std::env::var(PARENT_PID_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|pid| *pid > 1)
            .ok_or("supervision_required")?;
        // SAFETY: getppid takes no arguments and has no memory preconditions.
        if unsafe { libc::getppid() } != expected {
            return Err("parent_exited");
        }
        // SAFETY: this prctl option takes a signal number, not pointers. All
        // variadic arguments are explicitly the Linux unsigned-long ABI type.
        let installed = unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        if installed != 0 {
            return Err("parent_guard_unavailable");
        }
        // Covers parent-process death before/during installation: a signal is
        // not retroactively delivered for a parent that died before prctl.
        // Linux also signals on death of the creating THREAD; this is not a
        // process-only availability guarantee, nor inherited by grandchildren.
        // SAFETY: same argument-free getppid call as above.
        if unsafe { libc::getppid() } != expected {
            return Err("parent_exited");
        }
        Ok(())
    }
    pub(super) async fn invoke(
        source: String,
        input: Value,
        mut cancel: oneshot::Receiver<()>,
    ) -> Result<Value, HelperError> {
        let deadline = Instant::now() + CALL_BUDGET;
        let request = request(&source, &input)?;
        if cancelled(&mut cancel) {
            return Err(HelperError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(HelperError::Timeout);
        }
        // No caller-controlled executable, argv, cwd, environment or feature flag.
        let exe = std::env::current_exe().map_err(|_| HelperError::Spawn)?;
        let mut command = Builder::clean_cli(exe);
        command
            .env_clear()
            .env(PARENT_PID_ENV, std::process::id().to_string())
            .arg("--internal-extension-js")
            .stdin(Stdio::piped());
        let child = command.spawn().map_err(|_| HelperError::Spawn)?;
        wait_owned(child, request, cancel, deadline).await
    }
    async fn wait_owned(
        child: Child,
        request: Vec<u8>,
        cancel: oneshot::Receiver<()>,
        deadline: Instant,
    ) -> Result<Value, HelperError> {
        let (alive, abandoned) = oneshot::channel();
        let (answer, result) = oneshot::channel();
        // This task, not the caller future, owns Child and all pipe futures.
        tokio::spawn(async move {
            let outcome = supervise(child, request, cancel, abandoned, deadline).await;
            let _ = answer.send(outcome);
        });
        let outcome = result.await.map_err(|_| HelperError::OwnerStopped)?;
        drop(alive);
        outcome
    }
    async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, HelperError> {
        let mut bytes = Vec::new();
        reader
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| HelperError::Io)?;
        if bytes.len() > limit {
            return Err(HelperError::OutputLimit);
        }
        Ok(bytes)
    }
    async fn reap(child: &mut Child) -> Result<(), HelperError> {
        if child.id().is_some() && kill_process_tree(child).await.is_err() {
            // A group-kill failure must not skip direct-child kill and wait.
            let _ = child.start_kill();
        }
        child.wait().await.map(|_| ()).map_err(|_| HelperError::Cleanup)
    }
    async fn supervise(
        mut child: Child,
        request: Vec<u8>,
        mut cancel: oneshot::Receiver<()>,
        mut abandoned: oneshot::Receiver<()>,
        deadline: Instant,
    ) -> Result<Value, HelperError> {
        let outcome = {
            let pipes = async {
                let mut stdin = child.stdin.take().ok_or(HelperError::Io)?;
                let stdout = child.stdout.take().ok_or(HelperError::Io)?;
                let stderr = child.stderr.take().ok_or(HelperError::Io)?;
                let write = async move {
                    // An unavailable helper can close stdin before the write.
                    // Retain its bounded response instead of racing BrokenPipe.
                    let result = async {
                        stdin.write_all(&request).await?;
                        stdin.shutdown().await
                    }
                    .await;
                    drop(stdin);
                    Ok::<_, HelperError>(result)
                };
                let wait = async { child.wait().await.map_err(|_| HelperError::Io) };
                let (written, stdout, _stderr, status) = tokio::try_join!(
                    write,
                    read_bounded(stdout, STDOUT_MAX),
                    read_bounded(stderr, STDERR_MAX),
                    wait,
                )?;
                let response = decode(&stdout, status.success(), status.code());
                if matches!(response, Err(HelperError::Unavailable | HelperError::ParentGuard)) {
                    return response;
                }
                written.map_err(|_| HelperError::Io)?;
                response
            };
            tokio::pin!(pipes);
            tokio::select! {
                biased;
                _ = &mut cancel => Err(HelperError::Cancelled),
                _ = &mut abandoned => Err(HelperError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => Err(HelperError::Timeout),
                outcome = &mut pipes => outcome,
            }
        }; // All pipe futures are dropped/closed before process cleanup.
        if outcome.is_err() {
            reap(&mut child).await?;
        }
        outcome
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Success {
        ok: bool,
        result: Value,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Failure {
        ok: bool,
        error: String,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Reply {
        Success(Success),
        Failure(Failure),
    }
    fn decode(bytes: &[u8], success: bool, code: Option<i32>) -> Result<Value, HelperError> {
        let reply: Reply = serde_json::from_slice(bytes).map_err(|_| HelperError::Protocol)?;
        if let Reply::Failure(Failure { ok: false, ref error }) = reply
            && code == Some(2)
            && error == "runner_unavailable"
        {
            return Err(HelperError::Unavailable);
        }
        if let Reply::Failure(Failure { ok: false, ref error }) = reply
            && code == Some(2)
            && matches!(
                error.as_str(),
                "supervision_required" | "parent_exited" | "parent_guard_unavailable"
            )
        {
            return Err(HelperError::ParentGuard);
        }
        if !success {
            return Err(HelperError::ChildExit);
        }
        match reply {
            Reply::Success(Success { ok: true, result }) => {
                encode(&result, JSON_MAX).map_err(|_| HelperError::OutputLimit)?;
                Ok(result)
            }
            Reply::Failure(Failure { ok: false, error })
                if !error.is_empty()
                    && error.len() <= 64
                    && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
            {
                Err(HelperError::Execution(error))
            }
            _ => Err(HelperError::Protocol),
        }
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    #[tokio::test]
    async fn unavailable_without_starting_an_executable() {
        let (_sender, cancel) = oneshot::channel();
        assert_eq!(
            invoke("invalid JS".into(), Value::Null, cancel).await,
            Err(HelperError::Unavailable)
        );
    }
}
