//! Startup-time PATH enhancement.
//!
//! Call [`enhance_process_path`] from `main()` **before any worker thread
//! is spawned** (including the tokio runtime). It rewrites
//! `std::env::var("PATH")` to include:
//!
//! 1. The interactive login-shell `PATH` (Unix only, bounded by a 3s
//!    end-to-end budget) — fixes launchd / Finder / systemd-service starts.
//! 2. The current `PATH` (inherited from the launching process).
//! 3. Platform extra bins (`~/.cargo/bin`, `~/.local/bin`, etc.) as fallbacks.
//!
//! After this runs, all downstream `which::which(...)` and
//! `Command::new(...)` calls see the enhanced PATH with zero further
//! wiring.
//!
//! All of this runs *before* `init_tracing`, so nothing logged from this
//! module reaches a log sink. The probe therefore records a
//! [`ShellProbeReport`], which the bootstrap layer reads back through
//! [`login_shell_probe_report`] and emits once a subscriber exists.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(unix)]
use std::time::{Duration, Instant};

/// Total wall-clock budget for the login-shell PATH probe, covering both
/// draining the shell's stdout and reaping the process.
#[cfg(unix)]
const LOGIN_SHELL_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// How the startup login-shell PATH probe ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellProbeStatus {
    /// The probe produced a usable PATH.
    Ok,
    /// Not attempted: non-Unix, `$SHELL` unset, or `$SHELL` not absolute.
    Skipped,
    /// The login shell could not be spawned.
    SpawnFailed,
    /// The probe exceeded its budget and was killed.
    TimedOut,
    /// The shell ran but yielded no usable PATH: non-zero exit, empty
    /// output, or stdout could not be read.
    Unusable,
}

impl ShellProbeStatus {
    /// Stable, log-safe discriminant. Never carries PATH contents.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Skipped => "skipped",
            Self::SpawnFailed => "spawn_failed",
            Self::TimedOut => "timed_out",
            Self::Unusable => "unusable",
        }
    }
}

/// Log-safe outcome of the startup login-shell PATH probe.
///
/// Deliberately carries no PATH contents — only a status discriminant and
/// how long the probe took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellProbeReport {
    pub status: ShellProbeStatus,
    pub elapsed_ms: u64,
}

/// Written exactly once, by [`enhance_process_path`], under the same
/// single-threaded precondition that already governs that function.
static LOGIN_SHELL_PROBE_REPORT: OnceLock<ShellProbeReport> = OnceLock::new();

/// Outcome of the login-shell PATH probe run by [`enhance_process_path`].
///
/// That function runs before `init_tracing`, so it cannot log its own
/// result. The bootstrap layer calls this once a tracing subscriber exists
/// and emits the report there; a probe that times out would otherwise leave
/// no trace on any log sink. `None` means the probe has not run yet.
pub fn login_shell_probe_report() -> Option<ShellProbeReport> {
    LOGIN_SHELL_PROBE_REPORT.get().copied()
}

/// Enhance the current process's `PATH`. Returns the merged PATH string
/// for logging/debugging.
///
/// # Safety
///
/// Must be called **before** any other thread exists (including the
/// tokio runtime). Internally calls `std::env::set_var` which is
/// `unsafe` on Rust 2024.
pub unsafe fn enhance_process_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let (login, probe) = login_shell_path();
    // Nothing here can be logged yet; stash the outcome for the bootstrap
    // layer to emit once `init_tracing` has installed a subscriber.
    let _ = LOGIN_SHELL_PROBE_REPORT.set(probe);
    let extras = platform_extra_bins();

    let merged = merge_paths(&extras, &current, login.as_deref());

    if merged == current {
        tracing::warn!("PATH enhancement produced no changes; continuing with inherited PATH");
    } else {
        tracing::info!(
            login = login.is_some(),
            extra_bin_count = extras.len(),
            original_len = current.len(),
            merged_len = merged.len(),
            "PATH enhanced at startup"
        );
    }

    // SAFETY: caller guarantees single-threaded precondition.
    unsafe {
        std::env::set_var("PATH", &merged);
    }
    merged
}

// Placeholder helpers — filled in by later tasks.

fn merge_paths(extras: &[PathBuf], current: &str, login: Option<&str>) -> String {
    // Order: login, current, extras. First-occurrence wins.
    // Scanned version-manager bins are fallbacks and must not override the
    // Node version selected by the user's interactive login shell.
    // `env::split_paths` and `env::join_paths` honour the OS-specific
    // separator (':' on Unix, ';' on Windows) and handle quoting.
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut parts: Vec<PathBuf> = Vec::new();

    let mut push = |p: PathBuf| {
        if p.as_os_str().is_empty() {
            return;
        }
        if seen.insert(p.clone()) {
            parts.push(p);
        }
    };

    if let Some(l) = login {
        for p in std::env::split_paths(l) {
            push(p);
        }
    }
    for p in std::env::split_paths(current) {
        push(p);
    }
    for p in extras {
        push(p.clone());
    }

    std::env::join_paths(&parts)
        .map(|os| os.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn platform_extra_bins() -> Vec<PathBuf> {
    platform_extra_bins_at(dirs::home_dir().as_deref())
}

fn platform_extra_bins_at(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push_if_dir = |p: PathBuf| {
        if p.is_dir() {
            out.push(p);
        }
    };

    if let Some(h) = home {
        // Package-manager global bins.
        push_if_dir(h.join(".cargo").join("bin"));
        push_if_dir(h.join("go").join("bin"));
        push_if_dir(h.join(".deno").join("bin"));
        push_if_dir(h.join(".bun").join("bin"));
        push_if_dir(h.join(".local").join("bin"));
        push_if_dir(h.join(".volta").join("bin"));
        // Agent CLIs whose vendor installer uses a private directory rather
        // than one of the above (MiMo Code: `INSTALL_DIR=$HOME/.mimocode/bin`).
        push_if_dir(h.join(".mimocode").join("bin"));
        for nvm_bin in nvm_version_bins(h) {
            push_if_dir(nvm_bin);
        }
    }

    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            push_if_dir(PathBuf::from(&appdata).join("npm"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            push_if_dir(PathBuf::from(&local).join("pnpm"));
            push_if_dir(PathBuf::from(&local).join("fnm_multishells"));
            // winget package shims (stable since App Installer 1.4).
            push_if_dir(PathBuf::from(&local).join("Microsoft").join("WinGet").join("Links"));
            // Yarn classic global bin.
            push_if_dir(PathBuf::from(&local).join("Yarn").join("bin"));
            // omp's Windows installer writes `omp.exe` straight into this
            // directory (`install.ps1`: `$InstallDir = "$env:LOCALAPPDATA\omp"`).
            // It matters more here than the Unix vendor dirs do, because there
            // is no login-shell probe on Windows — this list plus the inherited
            // PATH is the whole search space.
            push_if_dir(PathBuf::from(&local).join("omp"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            push_if_dir(PathBuf::from(&pf).join("Git").join("cmd"));
            push_if_dir(PathBuf::from(&pf).join("Git").join("bin"));
            push_if_dir(PathBuf::from(&pf).join("nodejs"));
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            push_if_dir(PathBuf::from(&pf86).join("nodejs"));
        }
        if let Ok(scoop) = std::env::var("SCOOP") {
            push_if_dir(PathBuf::from(&scoop).join("shims"));
        } else if let Some(h) = home {
            push_if_dir(h.join("scoop").join("shims"));
        }
    }

    out
}

fn nvm_version_bins(home: &Path) -> Vec<PathBuf> {
    let versions_dir = home.join(".nvm").join("versions").join("node");
    let Ok(entries) = std::fs::read_dir(&versions_dir) else {
        return Vec::new();
    };

    let mut bins: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter(|bin| bin.is_dir())
        .collect();

    // Prefer newer-looking versions first, matching the user's active
    // Node installation ahead of older fallbacks when multiple bins exist.
    bins.sort_by(|a, b| b.cmp(a));
    bins
}

#[cfg(unix)]
fn login_shell_path() -> (Option<String>, ShellProbeReport) {
    let started = Instant::now();
    let report = |status: ShellProbeStatus| ShellProbeReport {
        status,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };

    let Ok(shell) = std::env::var("SHELL") else {
        return (None, report(ShellProbeStatus::Skipped));
    };
    if !Path::new(&shell).is_absolute() {
        tracing::debug!(%shell, "SHELL is not absolute, skipping login shell probe");
        return (None, report(ShellProbeStatus::Skipped));
    }

    let mut command = std::process::Command::new(&shell);
    command.args(["-l", "-i", "-c", "printf %s \"$PATH\""]);

    let (path, status) = probe_path_with_command(command, LOGIN_SHELL_PROBE_BUDGET);
    (path, report(status))
}

/// Run `command` and read a PATH string from its stdout, bounded end-to-end
/// by `budget`.
///
/// Split out from [`login_shell_path`] so tests can drive the exact process
/// shapes that break the probe without depending on the developer's own
/// `$SHELL` and rc files.
#[cfg(unix)]
fn probe_path_with_command(mut command: std::process::Command, budget: Duration) -> (Option<String>, ShellProbeStatus) {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;
    use wait_timeout::ChildExt;

    let deadline = Instant::now() + budget;

    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "login shell spawn failed");
            return (None, ShellProbeStatus::SpawnFailed);
        }
    };

    // Every exit path below reaps the child explicitly: `Child::drop` does
    // not wait, and leaving a zombie behind is not acceptable in a process
    // that is only starting up.
    let Some(mut stdout_handle) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return (None, ShellProbeStatus::Unusable);
    };

    // Drain stdout on a dedicated thread rather than inline.
    //
    // The read cannot simply move back after the wait: a PATH longer than the
    // pipe buffer would block the child's write while the parent blocks on
    // the wait. But reading inline is exactly what hung startup (AIONUI-150).
    // `read_to_string` returns only at EOF, and EOF requires *every* process
    // holding the pipe's write end to close it. An interactive login shell
    // (`-l -i`) sources the user's rc files, and any long-lived background
    // process those start (ssh-agent, gpg-agent, mise/direnv daemons, shell
    // update checks, ...) inherits this pipe and holds it open indefinitely.
    // The inline read then blocked forever, ahead of the `wait_timeout` that
    // was supposed to bound the probe — and before `init_tracing` had run, so
    // the process went silent on both log sinks until the client gave up.
    //
    // Reading off-thread keeps the pipe draining, so the write-side deadlock
    // stays fixed, while `recv_timeout` bounds the caller.
    let (tx, rx) = mpsc::channel();
    if let Err(e) = std::thread::Builder::new()
        .name("login-shell-path-probe".to_string())
        .spawn(move || {
            let mut buf = String::new();
            let _ = tx.send(stdout_handle.read_to_string(&mut buf).map(|_| buf));
        })
    {
        tracing::debug!(error = %e, "login shell probe reader thread spawn failed");
        let _ = child.kill();
        let _ = child.wait();
        return (None, ShellProbeStatus::SpawnFailed);
    }

    // On timeout the reader thread stays parked on the orphaned fd until
    // whatever holds the write end lets go. Leaking one parked thread is the
    // deliberate trade against hanging the whole process: a blocked `read(2)`
    // cannot be forced to return, so we do not try. The thread owns both its
    // `ChildStdout` and the channel sender, so it tears itself down once the
    // read finally completes.
    let stdout = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(stdout)) => stdout,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "login shell stdout read failed");
            let _ = child.kill();
            let _ = child.wait();
            return (None, ShellProbeStatus::Unusable);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            tracing::warn!("login shell PATH probe timed out while reading stdout");
            return (None, ShellProbeStatus::TimedOut);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            tracing::debug!("login shell probe reader thread ended without a result");
            let _ = child.kill();
            let _ = child.wait();
            return (None, ShellProbeStatus::Unusable);
        }
    };

    // Whatever the read left of the budget also has to cover reaping: a shell
    // that closes stdout but keeps running must not extend the probe either.
    let status = match child.wait_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            tracing::warn!("login shell PATH probe timed out while waiting for exit");
            return (None, ShellProbeStatus::TimedOut);
        }
        Err(e) => {
            tracing::debug!(error = %e, "login shell wait_timeout errored");
            let _ = child.kill();
            let _ = child.wait();
            return (None, ShellProbeStatus::Unusable);
        }
    };

    if !status.success() {
        tracing::debug!(?status, "login shell exited non-zero");
        return (None, ShellProbeStatus::Unusable);
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        (None, ShellProbeStatus::Unusable)
    } else {
        (Some(trimmed.to_string()), ShellProbeStatus::Ok)
    }
}

#[cfg(not(unix))]
fn login_shell_path() -> (Option<String>, ShellProbeReport) {
    // There is no login-shell probe on Windows: `platform_extra_bins` plus the
    // inherited PATH is the whole search space there.
    (
        None,
        ShellProbeReport {
            status: ShellProbeStatus::Skipped,
            elapsed_ms: 0,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sep() -> &'static str {
        if cfg!(windows) { ";" } else { ":" }
    }

    #[test]
    fn merge_paths_dedupes_preserve_order() {
        let s = sep();
        let current = format!("/a{s}/b{s}/c");
        let login = format!("/b{s}/d");
        let extras: Vec<PathBuf> = vec![PathBuf::from("/e")];

        let result = merge_paths(&extras, &current, Some(&login));
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/b", "/d", "/a", "/c", "/e"]);
    }

    #[test]
    fn merge_paths_prefers_login_nvm_bin_over_inherited_and_scanned_versions() {
        let s = sep();
        let active = "/home/user/.nvm/versions/node/v22.22.0/bin";
        let newer = PathBuf::from("/home/user/.nvm/versions/node/v25.1.0/bin");
        let current = format!("/opt/homebrew/bin{s}/usr/bin");
        let login = format!("{active}{s}/opt/homebrew/bin{s}/usr/bin");

        let result = merge_paths(&[newer.clone(), PathBuf::from(active)], &current, Some(&login));
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(
            parts,
            vec![active, "/opt/homebrew/bin", "/usr/bin", newer.to_str().unwrap()]
        );
    }

    #[test]
    fn merge_paths_drops_empty_segments() {
        let s = sep();
        let current = format!("{s}/a{s}{s}/b{s}");

        let result = merge_paths(&[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/a", "/b"]);
    }

    #[test]
    fn merge_paths_all_optional_none() {
        let result = merge_paths(&[], "", None);
        assert_eq!(result, "");
    }

    #[test]
    fn platform_extra_bins_at_filters_nonexistent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();

        // 构造少量"存在"的 bin 目录，其他 candidate 仍会被 platform_extra_bins_at
        // 检查但应被过滤掉。
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v22.22.0/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v25.1.0/bin")).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        // 至少这些目录应出现
        assert!(
            bins.iter().any(|p| p.ends_with(".cargo/bin")),
            "expected ~/.cargo/bin in result"
        );
        assert!(
            bins.iter().any(|p| p.ends_with(".nvm/versions/node/v22.22.0/bin")),
            "expected ~/.nvm/versions/node/v22.22.0/bin in result"
        );
        assert!(
            bins.iter().any(|p| p.ends_with(".nvm/versions/node/v25.1.0/bin")),
            "expected ~/.nvm/versions/node/v25.1.0/bin in result"
        );
        let nvm_bins: Vec<_> = bins
            .iter()
            .filter(|p| p.to_string_lossy().contains(".nvm/versions/node/"))
            .collect();
        assert_eq!(nvm_bins.len(), 2);
        assert!(
            nvm_bins[0].ends_with(".nvm/versions/node/v25.1.0/bin"),
            "expected newer NVM bin first"
        );
        assert!(
            nvm_bins[1].ends_with(".nvm/versions/node/v22.22.0/bin"),
            "expected older NVM bin second"
        );

        // 没创建的目录不应出现
        assert!(!bins.iter().any(|p| p.ends_with("go/bin")));
        assert!(!bins.iter().any(|p| p.ends_with(".deno/bin")));
    }

    /// bun sits alongside cargo/deno/volta as a package manager whose global
    /// installs land in a home-relative bin, and `.mimocode/bin` is where the
    /// MiMo Code installer puts its CLI. Both were missing from the fallback
    /// list, so an agent installed either way was only findable when the
    /// login-shell probe happened to succeed.
    #[test]
    fn platform_extra_bins_at_includes_bun_and_vendor_install_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();

        std::fs::create_dir_all(home.join(".bun/bin")).unwrap();
        std::fs::create_dir_all(home.join(".mimocode/bin")).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        assert!(
            bins.iter().any(|p| p.ends_with(".bun/bin")),
            "expected ~/.bun/bin in result: {bins:?}"
        );
        assert!(
            bins.iter().any(|p| p.ends_with(".mimocode/bin")),
            "expected ~/.mimocode/bin in result: {bins:?}"
        );
    }

    /// The list is a fallback, not a guess: a directory that does not exist
    /// must not be pushed onto PATH just because a vendor might use it.
    #[test]
    fn platform_extra_bins_at_omits_bun_and_vendor_dirs_that_do_not_exist() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();

        let bins = platform_extra_bins_at(Some(home));

        assert!(!bins.iter().any(|p| p.ends_with(".bun/bin")), "{bins:?}");
        assert!(!bins.iter().any(|p| p.ends_with(".mimocode/bin")), "{bins:?}");
    }

    #[test]
    fn platform_extra_bins_at_handles_no_home() {
        let bins = platform_extra_bins_at(None);
        // 没 home 时，Unix 返回空；Windows 可能仍从 env 读到 APPDATA 等——两种都可接受。
        // 只验证不 panic。
        let _ = bins;
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_returns_none_without_shell_var() {
        if !run_in_env_child(
            "shell_env::tests::login_shell_path_returns_none_without_shell_var",
            &[],
            &["SHELL"],
        ) {
            return;
        }
        let (result, report) = login_shell_path();
        assert!(result.is_none());
        assert_eq!(report.status, ShellProbeStatus::Skipped);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_rejects_relative_shell() {
        if !run_in_env_child(
            "shell_env::tests::login_shell_path_rejects_relative_shell",
            &[("SHELL", "sh")],
            &[],
        ) {
            return;
        }
        let (result, report) = login_shell_path();
        assert!(result.is_none());
        assert_eq!(report.status, ShellProbeStatus::Skipped);
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_roundtrip_with_sh() {
        if !run_in_env_child(
            "shell_env::tests::login_shell_path_roundtrip_with_sh",
            &[("SHELL", "/bin/sh")],
            &[],
        ) {
            return;
        }
        let (result, report) = login_shell_path();
        assert!(result.is_some(), "login shell probe should return Some");
        let path = result.unwrap();
        assert!(!path.is_empty(), "login shell PATH should not be empty");
        assert_eq!(report.status, ShellProbeStatus::Ok);
    }

    /// AIONUI-150 regression. The child prints its PATH and exits at once, but
    /// a long-lived grandchild inherits the stdout pipe and keeps the write end
    /// open, so the pipe never reaches EOF. The pre-fix inline `read_to_string`
    /// blocked there indefinitely — ahead of the `wait_timeout` that was meant
    /// to bound the probe, and before `init_tracing`, which is why production
    /// startups went silent on both log sinks until the 60s client deadline.
    ///
    /// The probe must now come back inside its own budget and report why.
    #[cfg(unix)]
    #[test]
    fn probe_is_bounded_when_a_grandchild_holds_stdout_open() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "sleep 20 & printf %s /opt/aionui/probe-bin"]);

        let started = Instant::now();
        let (path, status) = probe_path_with_command(command, Duration::from_millis(500));
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "probe must return within its budget, took {elapsed:?}"
        );
        assert_eq!(status, ShellProbeStatus::TimedOut);
        assert!(path.is_none(), "a timed-out probe must not contribute a PATH");
    }

    /// The single budget covers reaping as well as reading: a shell that closes
    /// stdout but keeps running must not extend the probe. This is the case the
    /// original `wait_timeout` was written for, and it has to keep holding now
    /// that the read has moved off-thread.
    #[cfg(unix)]
    #[test]
    fn probe_is_bounded_when_the_child_lingers_after_closing_stdout() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf %s /opt/aionui/probe-bin; exec 1>&-; exec sleep 20"]);

        let started = Instant::now();
        let (path, status) = probe_path_with_command(command, Duration::from_millis(500));
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "probe must return within its budget, took {elapsed:?}"
        );
        assert_eq!(status, ShellProbeStatus::TimedOut);
        assert!(path.is_none(), "a timed-out probe must not contribute a PATH");
    }

    /// The read runs before the wait so the pipe keeps draining: a PATH larger
    /// than the pipe buffer (64 KiB on Linux, 16 KiB on macOS) would otherwise
    /// block the child's write while the parent blocked on the wait. Moving the
    /// read onto a thread must not give that protection up.
    #[cfg(unix)]
    #[test]
    fn probe_reads_output_larger_than_the_pipe_buffer() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "i=0; while [ $i -lt 4096 ]; do printf '/opt/aionui/probe/padding/segment-%s:' \"$i\"; i=$((i+1)); done",
        ]);

        let (path, status) = probe_path_with_command(command, Duration::from_secs(10));

        assert_eq!(status, ShellProbeStatus::Ok, "large output must still succeed");
        let path = path.expect("a large PATH should still be returned");
        assert!(
            path.len() > 128 * 1024,
            "expected output well past the pipe buffer, got {} bytes",
            path.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_returns_trimmed_stdout_from_a_well_behaved_command() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf '  /opt/aionui/bin:/usr/bin  \\n'"]);

        let (path, status) = probe_path_with_command(command, Duration::from_secs(3));

        assert_eq!(status, ShellProbeStatus::Ok);
        assert_eq!(path.as_deref(), Some("/opt/aionui/bin:/usr/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_rejects_output_from_a_command_that_exits_non_zero() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf %s /opt/aionui/bin; exit 3"]);

        let (path, status) = probe_path_with_command(command, Duration::from_secs(3));

        assert_eq!(status, ShellProbeStatus::Unusable);
        assert!(path.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn probe_rejects_empty_output() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf ''"]);

        let (path, status) = probe_path_with_command(command, Duration::from_secs(3));

        assert_eq!(status, ShellProbeStatus::Unusable);
        assert!(path.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn probe_reports_spawn_failure_for_a_missing_binary() {
        let command = std::process::Command::new("/nonexistent/aionui-login-shell-probe");

        let (path, status) = probe_path_with_command(command, Duration::from_secs(3));

        assert_eq!(status, ShellProbeStatus::SpawnFailed);
        assert!(path.is_none());
    }

    /// These strings are a log contract read by whoever triages the next
    /// occurrence, and none of them may carry PATH data.
    #[test]
    fn probe_status_strings_are_stable() {
        assert_eq!(ShellProbeStatus::Ok.as_str(), "ok");
        assert_eq!(ShellProbeStatus::Skipped.as_str(), "skipped");
        assert_eq!(ShellProbeStatus::SpawnFailed.as_str(), "spawn_failed");
        assert_eq!(ShellProbeStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(ShellProbeStatus::Unusable.as_str(), "unusable");
    }

    #[cfg(unix)]
    fn run_in_env_child(test_name: &str, envs: &[(&str, &str)], removals: &[&str]) -> bool {
        const CHILD_ENV: &str = "AIONUI_RUNTIME_SHELL_ENV_TEST_CHILD";

        if std::env::var_os(CHILD_ENV).is_some() {
            return true;
        }

        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(CHILD_ENV, "1");
        for key in removals {
            command.env_remove(key);
        }
        for (key, value) in envs {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "child test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    }
}
