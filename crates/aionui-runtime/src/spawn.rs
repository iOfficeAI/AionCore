//! Opinionated wrapper around [`tokio::process::Command`] that centralises
//! cross-cutting concerns of child-process spawning across the workspace.
//!
//! Two construction flavours are provided:
//!
//! * [`Builder::new`] — for long-running agent CLIs whose stdio is owned
//!   by the caller (e.g. ACP SDK). Defaults to inherited stdio. Callers
//!   typically override to `piped()` to capture the streams.
//!
//! * [`Builder::clean_cli`] — for short-lived CLI tools whose output we
//!   capture and parse. Defaults to piped stdio plus `NO_COLOR=1` and
//!   `TERM=dumb` so ANSI escape codes do not leak into the captured
//!   output.
//!
//! Both flavours:
//! * set `kill_on_drop(true)` so a panicking / erroring caller cannot
//!   leave orphaned children;
//! * remove `NODE_OPTIONS`, `NODE_INSPECT`, `NODE_DEBUG`, `CLAUDECODE`
//!   so the child doesn't inherit debug/agent state that belongs to the
//!   parent (matches v1 `acpConnectors.ts::getCleanAgentEnv`).
//!
//! Enhanced `PATH` (including the bundled bun directory) is handled
//! once at process startup by [`crate::enhance_process_path`]; Builder
//! does not re-inject it.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::ResolvedCommand;
use crate::resolver::resolve_command_path;

struct ProgramPlan {
    program: OsString,
    args_prefix: Vec<OsString>,
}

/// Construction mode — determines default stdio + env extras.
#[derive(Debug, Clone, Copy)]
enum Mode {
    Default,
    CleanCli,
}

pub struct Builder {
    inner: Command,
    mode: Mode,
}

/// Force-kill a spawned child and wait for the direct child handle to exit.
///
/// On Unix, children spawned through [`Builder::new`] are process-group
/// leaders, so this targets that group to clean up descendants as well. On
/// Windows, this uses `taskkill /T` to terminate the process tree.
pub async fn kill_process_tree(child: &mut Child) -> io::Result<()> {
    let Some(pid) = child.id() else {
        return child.kill().await;
    };

    #[cfg(unix)]
    force_kill_process_tree(pid, Some(pid))?;
    #[cfg(windows)]
    kill_windows_process_tree(pid).await?;
    #[cfg(not(any(unix, windows)))]
    child.kill().await?;
    child.wait().await.map(|_| ())
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("mode", &self.mode)
            .field("command", self.inner.as_std())
            .finish()
    }
}

/// Renders the configured spawn as a shell-style preview (`cd … && env -u
/// X K=V <prog> <args>…`) suitable for logs and error messages. Format
/// comes for free from `std::process::Command`'s `Debug` impl.
impl std::fmt::Display for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.inner.as_std(), f)
    }
}

impl Builder {
    /// Builder for long-running agent subprocesses (ACP SDK, legacy CLI).
    ///
    /// Defaults:
    /// - stdio: inherit (callers typically override with `.stdin(piped())`
    ///   etc. when they need to own the streams)
    /// - `kill_on_drop(true)`
    /// - removes `NODE_OPTIONS`, `NODE_INSPECT`, `NODE_DEBUG`, `CLAUDECODE`
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        let mut inner = command_from_program(program.as_ref());
        inner.kill_on_drop(true);
        configure_platform_spawn(&mut inner);
        strip_pollution(&mut inner);
        Self {
            inner,
            mode: Mode::Default,
        }
    }

    /// Builder for short-lived CLI tools whose output we capture.
    ///
    /// Defaults:
    /// - stdio: all piped
    /// - `kill_on_drop(true)`
    /// - removes `NODE_OPTIONS`, `NODE_INSPECT`, `NODE_DEBUG`, `CLAUDECODE`
    /// - sets `NO_COLOR=1`, `TERM=dumb`
    pub fn clean_cli<S: AsRef<OsStr>>(program: S) -> Self {
        let mut inner = command_from_program(program.as_ref());
        inner
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("NO_COLOR", "1")
            .env("TERM", "dumb");
        configure_platform_spawn(&mut inner);
        strip_pollution(&mut inner);
        Self {
            inner,
            mode: Mode::CleanCli,
        }
    }

    /// Builder from a fully resolved command plan.
    ///
    /// This bypasses `resolve_command_path` and uses the provided
    /// `program + args_prefix + env` directly.
    pub fn from_resolved(resolved: &ResolvedCommand) -> Self {
        let mut inner = command_from_resolved_program(resolved.program.as_os_str());
        inner.kill_on_drop(true);
        configure_platform_spawn(&mut inner);
        strip_pollution(&mut inner);
        inner.args(&resolved.args_prefix);
        inner.envs(resolved.env.iter().cloned());
        Self {
            inner,
            mode: Mode::Default,
        }
    }

    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdin(cfg);
        self
    }

    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdout(cfg);
        self
    }

    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stderr(cfg);
        self
    }

    /// Spawn the process and return the standard `tokio::process::Child`.
    pub fn spawn(mut self) -> io::Result<Child> {
        self.inner.spawn()
    }

    /// Run to completion and collect stdout/stderr.
    pub async fn output(mut self) -> io::Result<std::process::Output> {
        self.inner.output().await
    }
}

fn command_from_program(program: &OsStr) -> Command {
    command_from_plan(plan_program(program))
}

fn command_from_resolved_program(program: &OsStr) -> Command {
    command_from_plan(plan_resolved_program(program))
}

fn command_from_plan(plan: ProgramPlan) -> Command {
    let mut command = Command::new(&plan.program);
    command.args(plan.args_prefix);
    command
}

fn plan_program(program: &OsStr) -> ProgramPlan {
    plan_resolved_program(&resolve_program(program))
}

fn plan_resolved_program(program: &OsStr) -> ProgramPlan {
    let program = program.to_os_string();
    match shell_wrap_windows_script(&program) {
        Some(plan) => plan,
        None => ProgramPlan {
            program,
            args_prefix: Vec::new(),
        },
    }
}

#[cfg(any(windows, test))]
fn shell_wrap_windows_script(program: &OsStr) -> Option<ProgramPlan> {
    let ext = Path::new(&program).extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("ps1") {
        return Some(ProgramPlan {
            program: "powershell".into(),
            args_prefix: vec![
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                program.into(),
            ],
        });
    }

    if ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat") {
        return Some(ProgramPlan {
            program: "cmd".into(),
            args_prefix: vec!["/d".into(), "/c".into(), program.into()],
        });
    }

    None
}

#[cfg(not(any(windows, test)))]
fn shell_wrap_windows_script(_program: &OsStr) -> Option<ProgramPlan> {
    None
}

fn strip_pollution(cmd: &mut Command) {
    cmd.env_remove("NODE_OPTIONS")
        .env_remove("NODE_INSPECT")
        .env_remove("NODE_DEBUG")
        .env_remove("CLAUDECODE");
}

#[cfg(unix)]
fn configure_platform_spawn(cmd: &mut Command) {
    // Start each child in its own process group so explicit teardown can
    // kill the whole subtree (CLI + MCP descendants) in one shot.
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn configure_platform_spawn(_cmd: &mut Command) {}

#[cfg(unix)]
fn force_kill_process_tree(pid: u32, process_group_id: Option<u32>) -> io::Result<()> {
    if let Some(group_id) = process_group_id.filter(|group_id| *group_id > 1) {
        let result = unsafe { libc::kill(-(group_id as i32), libc::SIGKILL) };
        if result == 0 {
            return Ok(());
        }

        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return kill_unix_target(pid as i32);
        }
        return Err(err);
    }

    kill_unix_target(pid as i32)
}

#[cfg(unix)]
fn kill_unix_target(target: i32) -> io::Result<()> {
    let result = unsafe { libc::kill(target, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err)
    }
}

#[cfg(windows)]
async fn kill_windows_process_tree(pid: u32) -> io::Result<()> {
    let pid_arg = pid.to_string();
    let mut cmd = Builder::clean_cli("taskkill");
    cmd.args(["/F", "/T", "/PID", pid_arg.as_str()]);
    let output = cmd.output().await?;
    if output.status.success() || output.status.code() == Some(128) {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "taskkill failed for pid {pid} (exit {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    )))
}

/// Resolve `program` through `resolve_command_path` so callers don't have
/// to. If the input already contains a path separator (relative or
/// absolute) we leave it alone — only bare command names go through
/// the resolver, where the bundled-bun shim and Windows `.cmd / .ps1 /
/// .bat` fallbacks live.
fn resolve_program(program: &OsStr) -> OsString {
    if let Some(s) = program.to_str()
        && !s.is_empty()
        && !s.contains('/')
        && !s.contains('\\')
        && let Some(path) = resolve_command_path(s)
    {
        return path.into_os_string();
    }
    program.to_os_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResolvedCommand;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn clean_cli_captures_stdout_and_strips_env_pollution() {
        if !crate::test_support::run_in_env_child(
            "spawn::tests::clean_cli_captures_stdout_and_strips_env_pollution",
            |command| {
                command.env("NODE_OPTIONS", "--inspect=9229").env("CLAUDECODE", "1");
            },
        ) {
            return;
        }

        // Ask the child to print NODE_OPTIONS + CLAUDECODE; Builder must
        // have removed them.
        let mut b = Builder::clean_cli("sh");
        b.arg("-c")
            .arg("echo \"NO:${NODE_OPTIONS:-unset} CC:${CLAUDECODE:-unset}\"");
        let output = b.output().await.unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("NO:unset"), "got: {stdout}");
        assert!(stdout.contains("CC:unset"), "got: {stdout}");
        assert!(output.status.success());
    }

    #[tokio::test]
    async fn clean_cli_sets_no_color_and_term_dumb() {
        let mut b = Builder::clean_cli("sh");
        b.arg("-c").arg("echo \"NC:${NO_COLOR:-unset} TERM:${TERM:-unset}\"");
        let output = b.output().await.unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("NC:1"), "got: {stdout}");
        assert!(stdout.contains("TERM:dumb"), "got: {stdout}");
    }

    #[tokio::test]
    async fn agent_allows_stdio_override() {
        // agent() defaults to inherit. Override to piped, then verify
        // we can capture output.
        let mut b = Builder::new("sh");
        b.arg("-c").arg("echo hello").stdout(Stdio::piped());
        let output = b.output().await.unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn agent_strips_env_pollution() {
        if !crate::test_support::run_in_env_child("spawn::tests::agent_strips_env_pollution", |command| {
            command.env("NODE_INSPECT", "9229").env("NODE_DEBUG", "*");
        }) {
            return;
        }

        let mut b = Builder::new("sh");
        b.arg("-c")
            .arg("echo \"NI:${NODE_INSPECT:-unset} ND:${NODE_DEBUG:-unset}\"")
            .stdout(Stdio::piped());
        let output = b.output().await.unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("NI:unset"), "got: {stdout}");
        assert!(stdout.contains("ND:unset"), "got: {stdout}");
    }

    #[tokio::test]
    async fn spawn_returns_child_with_pid() {
        let mut b = Builder::new("sh");
        b.arg("-c").arg("sleep 0.05");
        let mut child = b.spawn().unwrap();
        assert!(child.id().is_some());
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    #[test]
    fn resolved_command_builder_applies_prefix_and_env() {
        let resolved = ResolvedCommand {
            program: "/bin/echo".into(),
            args_prefix: vec!["hello".into()],
            env: vec![("NO_COLOR".into(), "1".into())],
        };

        let builder = Builder::from_resolved(&resolved);
        let preview = builder.to_string();
        assert!(
            preview.contains("hello"),
            "preview should include args prefix: {preview}"
        );
        assert!(preview.contains("NO_COLOR=\"1\"") || preview.contains("NO_COLOR=1"));
    }

    #[test]
    fn display_renders_shell_style_command() {
        let mut b = Builder::new("/usr/local/bin/bun");
        b.current_dir("/tmp/work dir")
            .env("FOO", "bar baz")
            .args(["x", "--flag", "with space"]);

        let preview = format!("{b}");
        // Format inherited from std Command Debug: `cd "..." && env -u X K=V "prog" "args"...`
        assert!(
            preview.starts_with(r#"cd "/tmp/work dir" &&"#),
            "missing cwd prefix: {preview}"
        );
        assert!(preview.contains("env "), "expected env section: {preview}");
        assert!(preview.contains(r#"FOO="bar baz""#), "FOO missing: {preview}");
        // strip_pollution unsets these
        assert!(
            preview.contains("-u NODE_OPTIONS"),
            "missing -u NODE_OPTIONS: {preview}"
        );
        assert!(preview.contains("-u CLAUDECODE"), "missing -u CLAUDECODE: {preview}");
        assert!(
            preview.contains(r#""/usr/local/bin/bun""#),
            "program missing: {preview}"
        );
        assert!(preview.contains(r#""--flag""#), "arg --flag missing: {preview}");
        assert!(preview.contains(r#""with space""#), "arg with space missing: {preview}");
    }

    #[test]
    fn windows_powershell_shim_is_wrapped_before_args() {
        let mut b = Builder::new(r"C:\Users\me\scoop\shims\opencode.ps1");
        b.args(["--print", "hello"]);

        let command = b.inner.as_std();
        assert_eq!(command.get_program(), OsStr::new("powershell"));
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(
            args,
            vec![
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                OsString::from(r"C:\Users\me\scoop\shims\opencode.ps1"),
                OsString::from("--print"),
                OsString::from("hello"),
            ]
        );
    }

    #[test]
    fn windows_cmd_and_bat_shims_are_wrapped_before_args() {
        let mut b = Builder::new(r"C:\Users\me\AppData\Roaming\npm\opencode.cmd");
        b.args(["run", "task"]);

        let command = b.inner.as_std();
        assert_eq!(command.get_program(), OsStr::new("cmd"));
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(
            args,
            vec![
                OsString::from("/d"),
                OsString::from("/c"),
                OsString::from(r"C:\Users\me\AppData\Roaming\npm\opencode.cmd"),
                OsString::from("run"),
                OsString::from("task"),
            ]
        );

        let mut b = Builder::new(r"C:\Users\me\AppData\Roaming\npm\opencode.bat");
        b.arg("--version");

        let command = b.inner.as_std();
        assert_eq!(command.get_program(), OsStr::new("cmd"));
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(
            args,
            vec![
                OsString::from("/d"),
                OsString::from("/c"),
                OsString::from(r"C:\Users\me\AppData\Roaming\npm\opencode.bat"),
                OsString::from("--version"),
            ]
        );
    }

    #[cfg(unix)]
    fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !is_pid_alive(pid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    fn is_pid_alive(pid: u32) -> bool {
        let result = unsafe { libc::kill(pid as i32, 0) };
        if result == 0 {
            return true;
        }
        !matches!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_process_tree_uses_cached_group_when_leader_has_exited() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_string_lossy().into_owned();

        let mut builder = Builder::new("sh");
        builder
            .args([
                "-c",
                "sleep 60 & child=$!; printf '%s' \"$child\" > \"$1\"; exit 0",
                "runtime-cached-group-cleanup",
                marker_path.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = builder.spawn().unwrap();
        let leader_pid = child.id().expect("leader pid should exist");
        let status = child.wait().await.unwrap();
        assert!(status.success(), "leader should exit before cleanup test");

        let child_pid: u32 = std::fs::read_to_string(marker.path())
            .expect("background child pid marker should exist")
            .trim()
            .parse()
            .expect("background child pid should be numeric");

        assert!(
            is_pid_alive(child_pid),
            "background child pid={child_pid} should still be alive"
        );

        force_kill_process_tree(leader_pid, Some(leader_pid)).expect("cached group kill should succeed");

        assert!(
            wait_for_pid_exit(child_pid, Duration::from_secs(5)),
            "background child pid={child_pid} should exit after cached group kill",
        );
    }
}
