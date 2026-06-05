//! Startup-time PATH enhancement.
//!
//! Call [`enhance_process_path`] from `main()` **before any worker thread
//! is spawned** (including the tokio runtime). It rewrites
//! `std::env::var("PATH")` to include:
//!
//! 1. An explicit bun override directory, if configured.
//! 2. Platform extra bins (`~/.bun/bin`, `~/.cargo/bin`, etc.).
//! 3. The current `PATH` (inherited from the launching process).
//! 4. The login-shell `PATH` (Unix only, 3s timeout) — fixes
//!    launchd / Finder / systemd-service starts.
//!
//! After this runs, all downstream `which::which(...)` and
//! `Command::new(...)` calls see the enhanced PATH with zero further
//! wiring.

use std::path::{Path, PathBuf};

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
    let login = login_shell_path();
    let extras = platform_extra_bins();
    let bun_dir = crate::bun_bin_dir();

    let merged = merge_paths(bun_dir.as_deref(), &extras, &current, login.as_deref());

    if merged == current {
        tracing::warn!("PATH enhancement produced no changes; continuing with inherited PATH");
    } else {
        tracing::info!(
            login = login.is_some(),
            extra_bin_count = extras.len(),
            bun_injected = bun_dir.is_some(),
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

fn merge_paths(bun_dir: Option<&Path>, extras: &[PathBuf], current: &str, login: Option<&str>) -> String {
    // Order: bun_dir, extras, current, login. First-occurrence wins.
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

    if let Some(p) = bun_dir {
        push(p.to_path_buf());
    }
    for p in extras {
        push(p.clone());
    }
    for p in std::env::split_paths(current) {
        push(p);
    }
    if let Some(l) = login {
        for p in std::env::split_paths(l) {
            push(p);
        }
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
        push_if_dir(h.join(".bun").join("bin"));
        push_if_dir(h.join(".cargo").join("bin"));
        push_if_dir(h.join("go").join("bin"));
        push_if_dir(h.join(".deno").join("bin"));
        push_if_dir(h.join(".local").join("bin"));
        push_if_dir(h.join(".volta").join("bin"));
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
fn login_shell_path() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use wait_timeout::ChildExt;

    let shell = std::env::var("SHELL").ok()?;
    if !Path::new(&shell).is_absolute() {
        tracing::debug!(%shell, "SHELL is not absolute, skipping login shell probe");
        return None;
    }

    let mut child = match Command::new(&shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%shell, error = %e, "login shell spawn failed");
            return None;
        }
    };

    // Take stdout handle before waiting to prevent pipe-buffer deadlock.
    // If the PATH string is long enough to fill the pipe buffer, the child
    // will block on write while we block on wait — causing a deadlock and
    // the 3s timeout to fire. Reading stdout first drains the buffer.
    let mut stdout_handle = child.stdout.take()?;
    let mut stdout = String::new();
    stdout_handle.read_to_string(&mut stdout).ok()?;

    let status = match child.wait_timeout(Duration::from_secs(3)) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            tracing::warn!("login shell PATH probe timed out after 3s");
            return None;
        }
        Err(e) => {
            tracing::debug!(error = %e, "login shell wait_timeout errored");
            return None;
        }
    };

    if !status.success() {
        tracing::debug!(?status, "login shell exited non-zero");
        return None;
    }

    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<String> {
    None
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

        let result = merge_paths(None, &extras, &current, Some(&login));
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/e", "/a", "/b", "/c", "/d"]);
    }

    #[test]
    fn merge_paths_with_bun_dir_at_front() {
        let s = sep();
        let current = format!("/a{s}/b");
        let bun = PathBuf::from("/bun");

        let result = merge_paths(Some(&bun), &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/bun", "/a", "/b"]);
    }

    #[test]
    fn merge_paths_drops_empty_segments() {
        let s = sep();
        let current = format!("{s}/a{s}{s}/b{s}");

        let result = merge_paths(None, &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/a", "/b"]);
    }

    #[test]
    fn merge_paths_all_optional_none() {
        let result = merge_paths(None, &[], "", None);
        assert_eq!(result, "");
    }

    #[test]
    fn merge_paths_bun_dir_deduplicates_if_already_in_current() {
        let s = sep();
        let current = format!("/bun{s}/a");
        let bun = PathBuf::from("/bun");

        let result = merge_paths(Some(&bun), &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        // /bun appears first (from bun_dir), then /a from current.
        // Second /bun (inside current) is dedup'd.
        assert_eq!(parts, vec!["/bun", "/a"]);
    }

    #[test]
    fn platform_extra_bins_at_filters_nonexistent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();

        // 构造少量"存在"的 bin 目录，其他 candidate 仍会被 platform_extra_bins_at
        // 检查但应被过滤掉。
        std::fs::create_dir_all(home.join(".bun/bin")).unwrap();
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v22.22.0/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v25.1.0/bin")).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        // 至少这两个应出现
        assert!(
            bins.iter().any(|p| p.ends_with(".bun/bin")),
            "expected ~/.bun/bin in result"
        );
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
        // SAFETY: single-threaded test process.
        unsafe {
            std::env::remove_var("SHELL");
        }
        let result = login_shell_path();
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_rejects_relative_shell() {
        // SAFETY: single-threaded test process.
        unsafe {
            std::env::set_var("SHELL", "sh");
        }
        let result = login_shell_path();
        assert!(result.is_none());
        unsafe {
            std::env::remove_var("SHELL");
        }
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_roundtrip_with_sh() {
        // SAFETY: single-threaded test process.
        unsafe {
            std::env::set_var("SHELL", "/bin/sh");
        }
        let result = login_shell_path();
        assert!(result.is_some(), "login shell probe should return Some");
        let path = result.unwrap();
        assert!(!path.is_empty(), "login shell PATH should not be empty");
        unsafe {
            std::env::remove_var("SHELL");
        }
    }
}
