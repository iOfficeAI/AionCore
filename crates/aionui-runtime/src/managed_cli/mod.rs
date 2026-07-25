//! Resolution of bundled claude/codex CLI binaries.
//!
//! claude/codex run through the direct-CLI session path. In a packaged app the
//! CLIs are shipped inside `managed-resources/cli/<name>/<version>/<target>/`
//! and this module resolves the running platform's main executable there. In
//! dev / non-bundled runs it returns `None` so the caller falls back to the
//! bare command name (resolved via PATH). This resolution is intentionally
//! separate from `cli_probe` (availability detection stays PATH-only).

use std::path::{Path, PathBuf};

use crate::managed_resources::{self, ManagedResourcesMode};

mod prepare;
pub use prepare::{ManagedCliError, PreparedCli, managed_cli_contract_for_export, prepare_managed_cli_to_root};

/// Pinned CLI versions — the single source of truth. Bumping a CLI = change the
/// constant and rebuild (managed-resources is replaced wholesale on app update).
pub const CLAUDE_CLI_VERSION: &str = "2.1.215";
pub const CODEX_CLI_VERSION: &str = "0.144.6";

/// The pinned version for a supported CLI name, or `None` for unknown names.
pub fn cli_version(name: &str) -> Option<&'static str> {
    match name {
        "claude" => Some(CLAUDE_CLI_VERSION),
        "codex" => Some(CODEX_CLI_VERSION),
        _ => None,
    }
}

/// The runtime key (`<os>-<arch>`) identifying the current platform's bundled
/// CLI subtree, mirroring node's `platform_spec` runtime_key values.
pub fn current_runtime_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        _ => None,
    }
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

/// Locate the main executable inside a materialized CLI root.
///
/// - claude ships as a single `claude[.exe]` at the root.
/// - codex ships under `vendor/<triple>/bin/codex[.exe]`; the triple directory
///   name is platform-dependent (e.g. gnu vs musl on linux), so it is discovered
///   by listing rather than hardcoded, alongside its sibling sidecars (rg / zsh
///   / codex-code-mode-host) which stay in the same vendor subtree.
fn main_executable_in(name: &str, root: &Path) -> Option<PathBuf> {
    match name {
        "claude" => {
            let candidate = root.join(format!("claude{}", exe_suffix()));
            candidate.is_file().then_some(candidate)
        }
        "codex" => {
            let vendor = root.join("vendor");
            for entry in std::fs::read_dir(&vendor).ok()?.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let candidate = entry.path().join("bin").join(format!("codex{}", exe_suffix()));
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Resolve the absolute path to the bundled CLI binary for `name`.
///
/// Returns `Some(path)` only in `Bundled` mode when the binary is present;
/// otherwise `None` (dev / Download mode, or a missing bundle), signalling the
/// caller to fall back to the bare command name resolved via PATH.
pub fn resolve_bundled_cli(name: &str) -> Option<PathBuf> {
    if !matches!(
        managed_resources::managed_resources_mode(),
        ManagedResourcesMode::Bundled
    ) {
        return None;
    }
    let version = cli_version(name)?;
    let target = current_runtime_key()?;
    for source in managed_resources::cli_sources(name, version, target) {
        if let Some(exe) = main_executable_in(name, &source.root) {
            tracing::info!(cli = name, version, path = %exe.display(), "resolved bundled cli");
            return Some(exe);
        }
    }
    // Bundled CLI is missing but we are in Bundled mode: the backend will spawn
    // the bare command name resolved via PATH. Surface which concrete binary
    // that resolves to (or `<none>` when PATH has no such command either) so the
    // fallback is diagnosable in production logs (which run at info level).
    let resolved = crate::resolve_command_path(name)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<none>".to_owned());
    tracing::warn!(
        cli = name,
        version,
        resolved = %resolved,
        "bundled cli missing in Bundled mode; falling back to PATH"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_version_returns_pins() {
        assert_eq!(cli_version("claude"), Some(CLAUDE_CLI_VERSION));
        assert_eq!(cli_version("codex"), Some(CODEX_CLI_VERSION));
        assert_eq!(cli_version("gemini"), None);
    }

    #[test]
    fn current_runtime_key_is_supported_on_this_host() {
        // CI/dev hosts are among the six supported targets.
        assert!(current_runtime_key().is_some());
    }

    #[test]
    fn main_executable_finds_claude_single_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join(format!("claude{}", exe_suffix()));
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
        assert_eq!(main_executable_in("claude", dir.path()), Some(exe));
    }

    #[test]
    fn main_executable_finds_codex_under_vendor_triple() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("vendor").join("aarch64-apple-darwin").join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let exe = bin.join(format!("codex{}", exe_suffix()));
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
        assert_eq!(main_executable_in("codex", dir.path()), Some(exe));
    }

    #[test]
    fn main_executable_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(main_executable_in("claude", dir.path()).is_none());
        assert!(main_executable_in("codex", dir.path()).is_none());
    }

    #[test]
    fn resolve_returns_none_in_download_mode() {
        managed_resources::set_managed_resources_mode(ManagedResourcesMode::Download);
        assert!(resolve_bundled_cli("claude").is_none());
        assert!(resolve_bundled_cli("codex").is_none());
    }
}
