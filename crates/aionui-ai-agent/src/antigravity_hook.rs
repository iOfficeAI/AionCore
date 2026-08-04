//! Writes the `hooks.json` that registers AionUi's own binary as agy's
//! PreToolUse hook. The file carries NO policy: every decision is made at
//! runtime by the user. Only the callback coordinates live on disk.

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

/// Remove a `hooks.json` this workspace may carry from an earlier build.
///
/// Needed when a conversation becomes a teammate: a hook left behind would keep
/// calling back for approval nobody is there to give, stalling the team. Absence
/// is the success case, so a missing file is not an error.
pub fn remove_hooks_json(workspace: &Path) {
    let path = workspace.join(AGENTS_DIR).join("hooks.json");
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::debug!(path = %path.display(), "antigravity: removed the approval hook"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(path = %path.display(), error = %e, "antigravity: could not remove hooks.json"),
    }
}

/// Write `<workspace>/.agents/hooks.json` registering `hook_binary` as the
/// PreToolUse gate for every tool.
///
/// The `matcher: "*"` is deliberate — AionUi decides per call, so agy must ask
/// about all of them.
///
/// `command` is a COMMAND LINE, not a bare path: agy word-splits it and honours
/// double quotes (verified against agy 1.1.8, including a path containing
/// spaces). Both details are load-bearing — without the subcommand agy would
/// launch the whole backend instead of the hook, and without the quotes any
/// install path containing a space would break word splitting.
pub fn write_hooks_json(workspace: &Path, hook_binary: &Path) -> std::io::Result<()> {
    let dir = workspace.join(AGENTS_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("hooks.json"), hooks_json_body(hook_binary).as_bytes())
}

/// The `hooks.json` contents, without writing them.
///
/// A session that starts in full auto installs no hook — but it may be switched
/// out of full auto later, and the backend has no way to rebuild this on its own
/// (it knows the workspace, not where AionUi's binary lives). Handing it the
/// prepared body lets it restore the gate when the user asks for it back.
pub fn hooks_json_body(hook_binary: &Path) -> String {
    // A COMMAND LINE, not a bare path: agy word-splits it and honours double
    // quotes (verified against 1.1.8, including a path containing spaces).
    // Without the subcommand agy would launch the whole backend; without the
    // quotes any install path with a space would break word splitting.
    let command = format!("\"{}\" {HOOK_SUBCOMMAND}", hook_binary.to_string_lossy());
    let body = serde_json::json!({
        AntigravityHookConfig::HOOK_NAME: {
            "enabled": true,
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": command,
                }],
            }],
        }
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_owned())
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

    #[test]
    fn registers_the_bridge_without_embedding_any_policy() {
        let dir = tempfile::tempdir().unwrap();
        write_hooks_json(dir.path(), Path::new("/opt/aionui/backend")).unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let entry = &v[AntigravityHookConfig::HOOK_NAME];
        assert_eq!(entry["enabled"], true);
        assert_eq!(entry["PreToolUse"][0]["matcher"], "*");
        assert_eq!(entry["PreToolUse"][0]["hooks"][0]["type"], "command");
        // A COMMAND LINE, not a bare path: without the subcommand agy would
        // launch the whole backend instead of the hook.
        assert_eq!(
            entry["PreToolUse"][0]["hooks"][0]["command"],
            "\"/opt/aionui/backend\" antigravity-hook"
        );

        // The decision must never be baked into the file: AionUi owns it at
        // runtime. A literal allow/deny here would silently bypass the user.
        assert!(!raw.contains("\"allow\""), "policy leaked into hooks.json");
        assert!(!raw.contains("\"deny\""), "policy leaked into hooks.json");
    }

    #[test]
    fn an_install_path_with_spaces_stays_one_argument() {
        // agy word-splits `command` and honours double quotes (verified). An
        // unquoted path like /Applications/Aion UI.app/... would be split into
        // two arguments and the hook would never run.
        let dir = tempfile::tempdir().unwrap();
        write_hooks_json(dir.path(), Path::new("/Apps/Aion UI/backend")).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v[AntigravityHookConfig::HOOK_NAME]["PreToolUse"][0]["hooks"][0]["command"],
            "\"/Apps/Aion UI/backend\" antigravity-hook"
        );
    }

    #[test]
    fn overwrites_a_stale_registration() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".agents")).unwrap();
        std::fs::write(dir.path().join(".agents/hooks.json"), r#"{"old":{"enabled":true}}"#).unwrap();

        write_hooks_json(dir.path(), Path::new("/opt/aionui/backend")).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".agents/hooks.json")).unwrap();
        assert!(!raw.contains("\"old\""), "a stale hook must not survive");
    }

    #[test]
    fn removing_the_hook_clears_a_previous_install() {
        // A conversation that becomes a teammate must not keep calling back for
        // approval nobody is there to give.
        let dir = tempfile::tempdir().unwrap();
        write_hooks_json(dir.path(), std::path::Path::new("/opt/aionui/aioncore")).unwrap();
        assert!(dir.path().join(".agents/hooks.json").exists());

        remove_hooks_json(dir.path());
        assert!(!dir.path().join(".agents/hooks.json").exists());
    }

    #[test]
    fn removing_an_absent_hook_is_not_an_error() {
        // Absence IS the desired state, so a workspace that never had one is fine.
        let dir = tempfile::tempdir().unwrap();
        remove_hooks_json(dir.path());
        assert!(!dir.path().join(".agents/hooks.json").exists());
    }
}
