//! Writes the `hooks.json` that registers AionUi's own binary as agy's
//! PreToolUse hook. The file carries NO policy: every decision is made at
//! runtime by the user. Only the callback coordinates live on disk.
//!
//! On Windows the JSON points at a `.cmd` wrapper instead of the binary
//! directly: agy runs handlers through `cmd /c` there (verified:
//! `~/.gemini/antigravity-cli/builtin/skills/agy-customizations/docs/hooks.md`
//! lines 129-133, "Hook Handler Fields"), and the quoted absolute path has been
//! reporter-observed reaching cmd literally (#4317, a regression of the
//! `\\?\`-verbatim-path fix #4095). Windows runtime is not exercised by our
//! tests. The wrapper holds the command line that actually runs.
//!
//! One known edge, deliberately not solved here: team members share the team
//! workspace, so they share this one `hooks.json` — a mode switch by any member
//! installs or removes the gate for all of them (`aionui-session`'s
//! `sync_permission_hook`, which syncs the Windows wrapper alongside the JSON).
//! agy merges multiple NAMED hooks (verified: hooks.md, "File Format"), so
//! per-conversation hook names are the eventual way out if that ever bites.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use aionui_api_types::AntigravityHookConfig;

/// Per-conversation tokens for the permission hook's callback.
///
/// The hook runs as a separate local process, so the callback endpoint cannot
/// authenticate it with a user session. It presents this token instead, which
/// is minted when the session is built and checked when the hook calls back —
/// without it any local process could approve tool execution on the user's
/// behalf.
#[derive(Debug, Default)]
pub struct HookTokenRegistry {
    tokens: Mutex<HashMap<String, String>>,
}

impl HookTokenRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint (or replace) the token for a conversation. Replacing matters: a
    /// rebuilt session must invalidate the previous session's hooks, so a stale
    /// hook process cannot answer for the new one.
    pub fn issue(&self, conversation_id: &str) -> String {
        let token = uuid::Uuid::now_v7().to_string();
        if let Ok(mut map) = self.tokens.lock() {
            map.insert(conversation_id.to_owned(), token.clone());
        }
        token
    }

    /// Constant-time-ish check. Returns false for an unknown conversation,
    /// which is the fail-closed answer.
    pub fn verify(&self, conversation_id: &str, presented: &str) -> bool {
        let Ok(map) = self.tokens.lock() else {
            return false;
        };
        map.get(conversation_id)
            .is_some_and(|expected| expected.as_bytes() == presented.as_bytes())
    }

    pub fn revoke(&self, conversation_id: &str) {
        if let Ok(mut map) = self.tokens.lock() {
            map.remove(conversation_id);
        }
    }
}

/// Directory agy scans for workspace customizations. It also accepts
/// `.agent/` / `_agents/` / `_agent/`, but we always provision the canonical
/// one.
const AGENTS_DIR: &str = ".agents";

/// Subcommand that turns our own binary into agy's hook process.
const HOOK_SUBCOMMAND: &str = "antigravity-hook";

/// Remove the files this module installs. Absence is the success case, so a
/// missing file is not an error.
///
/// Needed when a conversation becomes a teammate: a hook left behind would keep
/// calling back for approval nobody is there to give, stalling the team.
pub fn remove_hooks_json(workspace: &Path) {
    let dir = workspace.join(AGENTS_DIR);
    remove_file_if_present(&dir.join("hooks.json"), "the approval hook");
    remove_platform_files(&dir);
}

/// Remove the platform-specific companion file, if this platform installs one.
#[cfg(windows)]
fn remove_platform_files(dir: &Path) {
    remove_file_if_present(
        &dir.join(AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME),
        "the hook wrapper",
    );
}

/// Off Windows there is no companion file: agy runs the quoted binary itself.
#[cfg(not(windows))]
fn remove_platform_files(_dir: &Path) {}

fn remove_file_if_present(path: &Path, what: &str) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "antigravity: removed {what}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "antigravity: could not remove {what}"),
    }
}

/// Write `<workspace>/.agents/hooks.json` registering the hook for every tool,
/// plus any platform-specific file it points at (Windows: the `.cmd` wrapper).
///
/// The `matcher: "*"` is deliberate — AionUi decides per call, so agy must ask
/// about all of them.
pub fn write_hooks_json(workspace: &Path, hook_binary: &Path) -> std::io::Result<()> {
    let dir = workspace.join(AGENTS_DIR);
    std::fs::create_dir_all(&dir)?;
    // Before hooks.json: a hooks.json that points at a wrapper that was never
    // written is a broken gate, and the caller sees the error either way.
    write_platform_files(&dir, hook_binary)?;
    std::fs::write(dir.join("hooks.json"), hooks_json_body(hook_binary).as_bytes())
}

#[cfg(windows)]
fn write_platform_files(dir: &Path, hook_binary: &Path) -> std::io::Result<()> {
    std::fs::write(
        dir.join(AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME),
        wrapper_body(hook_binary)?,
    )
}

/// Off Windows there is no companion file: agy runs the quoted binary itself.
#[cfg(not(windows))]
fn write_platform_files(_dir: &Path, _hook_binary: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Contents of the Windows wrapper: one `@`-silenced command line.
///
/// The path goes through `dunce::canonicalize`, not `std::fs`: on Windows the
/// latter returns a `\\?\`-prefixed verbatim path that cmd.exe is reported not
/// to launch (#4095). A failure to canonicalize is PROPAGATED, never papered
/// over with the original path — falling back could write exactly the verbatim
/// form this wrapper exists to keep out. Callers hand us a canonical path
/// today, but this module should not depend on caller discipline: it is the
/// thing that writes a file cmd will execute.
///
/// Compiled on Windows for production and in tests everywhere, so the format
/// can be pinned without a Windows host; nothing off Windows ever writes it.
#[cfg(any(windows, test))]
fn wrapper_body(hook_binary: &Path) -> std::io::Result<String> {
    let exe = dunce::canonicalize(hook_binary)?;
    Ok(format!("@\"{}\" {HOOK_SUBCOMMAND}\r\n", exe.display()))
}

/// The wrapper's contents, prepared for a later mode switch but NOT written:
/// `None` off Windows (agy runs the quoted binary directly), `Some(contents)`
/// on Windows, or an error when the binary cannot be canonicalized.
///
/// The factory carries this beside the prepared `hooks.json` body so the
/// session layer can restore the whole gate when a session leaves full auto —
/// it knows the workspace, not where AionUi's binary lives, so it cannot
/// rebuild either file on its own. Falling back to the un-canonicalized path is
/// deliberately not an option: on Windows that may be the `\\?\` verbatim form
/// the wrapper exists to keep out (#4095, reporter-observed).
pub fn prepare_wrapper_body(hook_binary: &Path) -> std::io::Result<Option<String>> {
    #[cfg(windows)]
    {
        Ok(Some(wrapper_body(hook_binary)?))
    }
    #[cfg(not(windows))]
    {
        let _ = hook_binary;
        Ok(None)
    }
}

/// The `hooks.json` contents, without writing them.
///
/// A session that starts in full auto installs no hook — but it may be switched
/// out of full auto later, and the backend has no way to rebuild this on its own
/// (it knows the workspace, not where AionUi's binary lives). Handing it the
/// prepared body lets it restore the gate when the user asks for it back.
pub fn hooks_json_body(hook_binary: &Path) -> String {
    hooks_json_body_with(hook_binary, cfg!(windows))
}

/// The body for a given platform. Split from [`hooks_json_body`] so both
/// branches are testable on any host: the Windows branch decides what agy will
/// run, but the Windows RUNTIME is not exercised without a Windows tester.
fn hooks_json_body_with(hook_binary: &Path, windows: bool) -> String {
    let body = serde_json::json!({
        AntigravityHookConfig::HOOK_NAME: {
            "enabled": true,
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": hook_command(hook_binary, windows),
                    // agy's default handler timeout is 30 s (verified: hooks.md,
                    // "Hook Handler Fields") — shorter than our own callback
                    // wait, so without this agy kills the hook while the user is
                    // still looking at the permission card. Outliving the wait
                    // lets the hook answer for itself, fail-closed.
                    "timeout": AntigravityHookConfig::HANDLER_TIMEOUT_SECS,
                }],
            }],
        }
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_owned())
}

/// The `command` agy runs for this workspace.
fn hook_command(hook_binary: &Path, windows: bool) -> String {
    if windows {
        // Bare file name, resolved against the handler's working directory —
        // the directory containing hooks.json (verified: hooks.md lines
        // 129-133, "Hook Handler Fields"). The quoted absolute path has been
        // reporter-observed reaching cmd literally (#4317), and a `\\?\`
        // verbatim path is reported not to launch (#4095). The wrapper carries
        // the real command line.
        AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME.to_owned()
    } else {
        // A COMMAND LINE, not a bare path: agy word-splits it and honours
        // double quotes (verified against agy 1.1.8, including a path
        // containing spaces). Both details are load-bearing — without the
        // subcommand agy would launch the whole backend instead of the hook,
        // and without the quotes any install path with a space would break
        // word splitting.
        format!("\"{}\" {HOOK_SUBCOMMAND}", hook_binary.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_conversation_never_verifies() {
        let reg = HookTokenRegistry::new();
        assert!(
            !reg.verify("conv-nope", "anything"),
            "unknown conversation must fail closed"
        );
    }

    #[test]
    fn issued_token_verifies_and_a_wrong_one_does_not() {
        let reg = HookTokenRegistry::new();
        let token = reg.issue("conv-1");
        assert!(reg.verify("conv-1", &token));
        assert!(!reg.verify("conv-1", "wrong"));
        // Tokens are per conversation: one session's hook cannot answer for another.
        assert!(!reg.verify("conv-2", &token));
    }

    #[test]
    fn reissue_invalidates_the_previous_token() {
        // A rebuilt session must not be answerable by a stale hook process.
        let reg = HookTokenRegistry::new();
        let old = reg.issue("conv-1");
        let new = reg.issue("conv-1");
        assert!(!reg.verify("conv-1", &old));
        assert!(reg.verify("conv-1", &new));
    }

    #[test]
    fn revoked_token_stops_verifying() {
        let reg = HookTokenRegistry::new();
        let token = reg.issue("conv-1");
        reg.revoke("conv-1");
        assert!(!reg.verify("conv-1", &token));
    }

    /// A real file to hand `write_hooks_json` as the binary. Windows
    /// canonicalizes it before embedding it in the wrapper, so a nonexistent
    /// path fails the install there by design (see
    /// `a_failed_wrapper_write_leaves_no_hooks_json_behind`).
    fn stub_exe(dir: &Path) -> std::path::PathBuf {
        let exe = dir.join("aioncore.exe");
        std::fs::write(&exe, b"stub").unwrap();
        exe
    }

    #[test]
    fn registers_the_bridge_without_embedding_any_policy() {
        let dir = tempfile::tempdir().unwrap();
        let exe = stub_exe(dir.path());
        write_hooks_json(dir.path(), &exe).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &v[AntigravityHookConfig::HOOK_NAME];
        assert_eq!(entry["enabled"], true);
        assert_eq!(entry["PreToolUse"][0]["matcher"], "*");
        assert_eq!(entry["PreToolUse"][0]["hooks"][0]["type"], "command");
        // Platform-dependent: the quoted binary on Unix, the wrapper name on
        // Windows. Either way without the subcommand agy would launch the whole
        // backend instead of the hook.
        assert_eq!(
            entry["PreToolUse"][0]["hooks"][0]["command"],
            hook_command(&exe, cfg!(windows))
        );

        // The decision must never be baked into the file: AionUi owns it at
        // runtime. A literal allow/deny here would silently bypass the user.
        assert!(!raw.contains("\"allow\""), "policy leaked into hooks.json");
        assert!(!raw.contains("\"deny\""), "policy leaked into hooks.json");
    }

    #[test]
    fn an_install_path_with_spaces_stays_one_argument() {
        // Unix form: agy word-splits `command` and honours double quotes
        // (verified). An unquoted path like /Applications/Aion UI.app/... would
        // be split into two arguments and the hook would never run. On Windows
        // the command is the wrapper name instead; see
        // `the_windows_command_is_the_bare_wrapper_name`.
        let raw = hooks_json_body_with(Path::new("/Apps/Aion UI/backend"), false);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v[AntigravityHookConfig::HOOK_NAME]["PreToolUse"][0]["hooks"][0]["command"],
            "\"/Apps/Aion UI/backend\" antigravity-hook"
        );
    }

    #[test]
    fn the_windows_command_is_the_bare_wrapper_name() {
        // On Windows the JSON must not carry the binary path at all: the quoted
        // form is reporter-observed reaching cmd literally (#4317), so agy runs
        // the wrapper — a bare file name resolved against the hooks.json
        // directory, which is the handler's documented working directory
        // (verified: hooks.md lines 129-133, "Hook Handler Fields"). What
        // Windows itself does with this is not exercised here.
        let raw = hooks_json_body_with(Path::new(r"C:\Program Files\AionUi\aioncore.exe"), true);
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v[AntigravityHookConfig::HOOK_NAME]["PreToolUse"][0]["hooks"][0]["command"],
            AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME
        );
    }

    #[test]
    fn the_handler_timeout_outlives_our_own_wait() {
        // agy's default handler timeout is 30 s (verified: hooks.md, "Hook
        // Handler Fields") and it kills the hook there. Our wait must end
        // first, or an unanswered card becomes a dead process instead of a
        // named denial.
        assert!(AntigravityHookConfig::CALLBACK_TIMEOUT.as_secs() > 30);
        assert!(AntigravityHookConfig::HANDLER_TIMEOUT_SECS > AntigravityHookConfig::CALLBACK_TIMEOUT.as_secs());

        for windows in [false, true] {
            let raw = hooks_json_body_with(Path::new("/opt/aionui/backend"), windows);
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                v[AntigravityHookConfig::HOOK_NAME]["PreToolUse"][0]["hooks"][0]["timeout"],
                AntigravityHookConfig::HANDLER_TIMEOUT_SECS,
                "windows={windows}"
            );
        }
    }

    #[test]
    fn the_wrapper_runs_the_binary_with_the_subcommand() {
        // Pinned on every host so the format is reviewable without a Windows
        // machine; what a Windows cmd does with it is not exercised here.
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aioncore.exe");
        std::fs::write(&exe, b"stub").unwrap();
        let canonical = dunce::canonicalize(&exe).unwrap();

        assert_eq!(
            wrapper_body(&exe).unwrap(),
            format!("@\"{}\" antigravity-hook\r\n", canonical.display())
        );
    }

    #[test]
    fn a_wrapper_that_cannot_be_canonicalized_is_an_error_not_a_fallback() {
        // The original path is never silently restored: on Windows it may be
        // the `\\?\` verbatim form this wrapper exists to keep out (#4095).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.exe");
        let err = wrapper_body(&missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn wrapper_preparation_is_platform_correct_and_fallible() {
        // The restore path prepares the wrapper only on the platform that
        // installs one: Some on Windows, None off it. A binary that cannot be
        // canonicalized must error on Windows — never a fallback path — and is
        // irrelevant off it.
        let dir = tempfile::tempdir().unwrap();
        let exe = stub_exe(dir.path());

        let prepared = prepare_wrapper_body(&exe).unwrap();
        assert_eq!(
            prepared.is_some(),
            cfg!(windows),
            "wrapper presence must match the platform"
        );
        if let Some(body) = prepared {
            assert!(
                body.contains("antigravity-hook"),
                "wrapper must carry the subcommand: {body}"
            );
            assert!(body.starts_with("@\""), "wrapper must silence the command line: {body}");
        }

        let missing = dir.path().join("does-not-exist.exe");
        match prepare_wrapper_body(&missing) {
            Ok(None) => assert!(
                !cfg!(windows),
                "windows must not prepare a wrapper for a missing binary"
            ),
            Ok(Some(_)) => panic!("a missing binary must never produce a wrapper"),
            Err(e) => {
                assert!(cfg!(windows), "only windows canonicalizes: {e}");
                assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_install_writes_a_wrapper_hooks_json_can_launch() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aioncore.exe");
        std::fs::write(&exe, b"stub").unwrap();

        write_hooks_json(dir.path(), &exe).unwrap();

        let wrapper = std::fs::read_to_string(
            dir.path()
                .join(".agents")
                .join(AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME),
        )
        .unwrap();
        assert_eq!(
            wrapper,
            format!(
                "@\"{}\" antigravity-hook\r\n",
                dunce::canonicalize(&exe).unwrap().display()
            )
        );
        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v[AntigravityHookConfig::HOOK_NAME]["PreToolUse"][0]["hooks"][0]["command"],
            AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_wrapper_write_leaves_no_hooks_json_behind() {
        // hooks.json must never point at a wrapper that was not written, so the
        // wrapper goes first and its failure aborts the install.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.exe");
        let err = write_hooks_json(dir.path(), &missing).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!dir.path().join(".agents/hooks.json").exists());
        assert!(
            !dir.path()
                .join(".agents")
                .join(AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME)
                .exists()
        );
    }

    #[test]
    fn overwrites_a_stale_registration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        std::fs::write(dir.path().join(".agents/hooks.json"), r#"{"old":{"enabled":true}}"#).unwrap();

        write_hooks_json(dir.path(), &stub_exe(dir.path())).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        assert!(!raw.contains("\"old\""), "a stale hook must not survive");
    }

    #[test]
    fn writing_installs_the_platform_files_and_removing_clears_them() {
        // A conversation that becomes a teammate must not keep calling back for
        // approval nobody is there to give, and a workspace must not keep files
        // this module installed.
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir
            .path()
            .join(".agents")
            .join(AntigravityHookConfig::WINDOWS_WRAPPER_FILE_NAME);

        write_hooks_json(dir.path(), &stub_exe(dir.path())).unwrap();
        assert!(dir.path().join(".agents/hooks.json").exists());
        // The wrapper belongs to the Windows install; off Windows agy runs the
        // quoted binary directly, so no wrapper may appear.
        assert_eq!(
            wrapper.exists(),
            cfg!(windows),
            "wrapper presence must match the platform"
        );

        remove_hooks_json(dir.path());
        assert!(!dir.path().join(".agents/hooks.json").exists());
        assert!(!wrapper.exists(), "the wrapper must go with hooks.json");
    }

    #[test]
    fn removing_an_absent_hook_is_not_an_error() {
        // Absence IS the desired state, so a workspace that never had one is fine.
        let dir = tempfile::tempdir().unwrap();
        remove_hooks_json(dir.path());
        assert!(!dir.path().join(".agents/hooks.json").exists());
    }
}
