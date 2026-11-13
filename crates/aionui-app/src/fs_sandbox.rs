//! File-sandbox policy for `/api/fs/*`, the office preview routes and project
//! registration.
//!
//! The defaults suit the desktop app, where the HTTP client is the local user:
//! the sandbox roots are the temp dir, the home dir and the work dir, and a
//! request may widen them with the `root` / `workspace` it sends (the folder the
//! user picked). A deployment that serves several users from one process (a
//! hosted or shared workspace) can tighten both with environment variables:
//!
//! - `AIONUI_FS_ALLOWED_ROOTS`: the sandbox roots, separated like `PATH`
//!   (`:` on Unix, `;` on Windows). Replaces the defaults when set and non-empty.
//! - `AIONUI_FS_STRICT_CLIENT_ROOTS=1` (or `true`): a request-supplied root never
//!   widens the sandbox, and a folder can only be registered as a project
//!   workspace or attached folder inside the roots. Without this, a client could
//!   name any directory as its `root`, or register it as a project, and then read
//!   and write anywhere the server process can.

use std::path::{Path, PathBuf};

const ALLOWED_ROOTS_ENV: &str = "AIONUI_FS_ALLOWED_ROOTS";
const STRICT_CLIENT_ROOTS_ENV: &str = "AIONUI_FS_STRICT_CLIENT_ROOTS";

/// The sandbox roots: `AIONUI_FS_ALLOWED_ROOTS` if set, otherwise the defaults.
pub(crate) fn allowed_roots(work_dir: Option<&Path>) -> Vec<PathBuf> {
    allowed_roots_from(std::env::var_os(ALLOWED_ROOTS_ENV).as_deref(), work_dir)
}

/// Whether request-supplied roots are trusted (`AIONUI_FS_STRICT_CLIENT_ROOTS` unset or false).
pub(crate) fn trust_client_roots() -> bool {
    !is_truthy(std::env::var(STRICT_CLIENT_ROOTS_ENV).ok().as_deref())
}

fn allowed_roots_from(configured: Option<&std::ffi::OsStr>, work_dir: Option<&Path>) -> Vec<PathBuf> {
    if let Some(value) = configured {
        let roots: Vec<PathBuf> = std::env::split_paths(value)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    default_allowed_roots(work_dir)
}

fn is_truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn default_allowed_roots(work_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![
        std::env::temp_dir(),
        dirs::home_dir().unwrap_or_else(std::env::temp_dir),
    ];
    // Auto-provisioned per-conversation workspaces live under
    // `{work_dir}/conversations/{label}-temp-{id}/`. On Windows the
    // operator may put `work_dir` on a separate drive (e.g. `X:\AionUi`)
    // that's neither under `temp_dir` nor `home_dir`. Including `work_dir`
    // keeps temp workspaces on the default allowlist without widening it
    // to unrelated paths.
    if let Some(wd) = work_dir
        && !wd.as_os_str().is_empty()
        && !roots.iter().any(|r| r == wd)
    {
        roots.push(wd.to_path_buf());
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_temp_home_and_work_dir() {
        let wd = PathBuf::from("/srv/work");
        let roots = allowed_roots_from(None, Some(&wd));
        assert!(roots.contains(&std::env::temp_dir()));
        assert!(roots.contains(&wd));
    }

    #[test]
    fn configured_roots_replace_the_defaults() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let value = format!("/workspace{sep}/tmp/aionui");
        let roots = allowed_roots_from(Some(std::ffi::OsStr::new(&value)), Some(Path::new("/srv/work")));
        assert_eq!(roots, vec![PathBuf::from("/workspace"), PathBuf::from("/tmp/aionui")]);
    }

    #[test]
    fn empty_configured_roots_fall_back_to_the_defaults() {
        let roots = allowed_roots_from(Some(std::ffi::OsStr::new("")), None);
        assert!(roots.contains(&std::env::temp_dir()));
    }

    #[test]
    fn strict_flag_values() {
        for v in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(Some(v)), "{v}");
        }
        for v in ["0", "false", "", "no"] {
            assert!(!is_truthy(Some(v)), "{v}");
        }
        assert!(!is_truthy(None));
    }
}
