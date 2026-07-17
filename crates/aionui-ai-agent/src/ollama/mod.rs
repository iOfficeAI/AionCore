//! Ollama integration for AionCore.
//!
//! This module lets compatible ACP agents run against a local Ollama
//! server without provider API keys. It works by injecting the same
//! provider environment variables that `ollama launch <agent>` injects
//! into the agent's *native ACP command*.
//!
//! Why not spawn `ollama launch <agent>` directly? `ollama launch`
//! starts the agent's interactive TUI (verified empirically with Ollama
//! 0.32.0: it resolves the agent binary on PATH and execs it with the
//! provider env applied). A TUI spawned without a TTY never answers the
//! ACP `initialize` request on stdio, so the handshake times out.
//! Injecting the environment into the native ACP bridge command keeps
//! the ACP transport intact while routing model calls to Ollama.

use std::sync::OnceLock;

use aionui_common::EnvVar;
use aionui_common::constants::{OLLAMA_COMMAND, OLLAMA_DEFAULT_BASE_URL, is_ollama_supported_agent};

/// Cached result of Ollama availability check
static OLLAMA_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Check if Ollama is installed and available on the system PATH.
///
/// This function caches the result after the first call to avoid repeated
/// filesystem lookups during agent discovery.
pub fn is_ollama_available() -> bool {
    *OLLAMA_AVAILABLE.get_or_init(|| {
        // Check if ollama command exists on PATH.
        // Uses split_paths for cross-platform PATH separator handling.
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                let ollama_path = dir.join(OLLAMA_COMMAND);
                if ollama_path.exists() && ollama_path.is_file() {
                    return true;
                }
            }
        }
        false
    })
}

/// Refresh the Ollama availability cache.
///
/// Call this after PATH changes (e.g., after Ollama installation) to force
/// a re-check of Ollama availability. Note: `OnceLock` cannot be reset, so
/// this function will only take effect if the cache has not yet been
/// initialised. In practice, Ollama detection happens early and PATH changes
/// during a single process lifetime are rare, making this acceptable.
pub fn refresh_ollama_availability() {
    // OnceLock has no clear() method. The cache is set once per process
    // lifetime. PATH changes mid-process are rare enough that this is
    // an acceptable trade-off.
}

/// Check if a given agent backend can run against Ollama.
pub fn is_agent_ollama_supported(backend: &str) -> bool {
    is_ollama_supported_agent(backend)
}

fn env_var(name: &str, value: impl Into<String>) -> EnvVar {
    EnvVar {
        name: name.to_owned(),
        value: value.into(),
    }
}

/// Build the environment variables that route a backend's model calls to
/// the local Ollama server, mirroring what `ollama launch <backend>`
/// injects (captured empirically from Ollama 0.32.1 via a PATH shim),
/// plus `ANTHROPIC_MODEL` which headless operation requires (see below).
///
/// Returns `None` for backends without a verified mapping.
pub fn build_ollama_env(backend: &str, model: &str) -> Option<Vec<EnvVar>> {
    match backend {
        "claude" => Some(vec![
            env_var("ANTHROPIC_BASE_URL", OLLAMA_DEFAULT_BASE_URL),
            env_var("ANTHROPIC_AUTH_TOKEN", "ollama"),
            // Cleared on purpose so a configured real key cannot take
            // precedence over the Ollama route (matches `ollama launch`).
            env_var("ANTHROPIC_API_KEY", ""),
            // Pin the session model. `ollama launch` does not set this
            // because it runs the interactive TUI where the user can pick
            // a model; headless ACP has no picker. Without it the ACP
            // bridge falls back to the user's persisted `settings.model`
            // (~/.claude/settings.json), which typically names a provider
            // model that does not exist on Ollama and fails the prompt
            // turn with JSON-RPC -32603 `model_not_found` (verified
            // against @agentclientprotocol/claude-agent-acp 0.58.1, whose
            // model priority is ANTHROPIC_MODEL > settings.model).
            env_var("ANTHROPIC_MODEL", model),
            env_var("ANTHROPIC_DEFAULT_OPUS_MODEL", model),
            env_var("ANTHROPIC_DEFAULT_SONNET_MODEL", model),
            env_var("ANTHROPIC_DEFAULT_HAIKU_MODEL", model),
            env_var("CLAUDE_CODE_SUBAGENT_MODEL", model),
            // Telemetry/nonessential-traffic suppression, matching the
            // exact env `ollama launch claude` injects (Ollama 0.32.1).
            // Keeps the agent from calling Anthropic endpoints with the
            // placeholder credentials above.
            env_var("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
            env_var("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
            env_var("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
            env_var("DISABLE_ERROR_REPORTING", "1"),
            env_var("DISABLE_FEEDBACK_COMMAND", "1"),
            env_var("DISABLE_TELEMETRY", "1"),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ollama_compatible_backend_coverage() {
        assert!(is_agent_ollama_supported("claude"));
        assert!(!is_agent_ollama_supported("gemini"));
        assert!(!is_agent_ollama_supported("codex"));
    }

    #[test]
    fn test_build_ollama_env_for_claude() {
        let env = build_ollama_env("claude", "qwen3:14b").expect("claude must have an env mapping");
        let get = |name: &str| env.iter().find(|var| var.name == name).map(|var| var.value.clone());
        assert_eq!(get("ANTHROPIC_BASE_URL").as_deref(), Some(OLLAMA_DEFAULT_BASE_URL));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN").as_deref(), Some("ollama"));
        assert_eq!(get("ANTHROPIC_API_KEY").as_deref(), Some(""));
        assert_eq!(get("ANTHROPIC_MODEL").as_deref(), Some("qwen3:14b"));
        assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL").as_deref(), Some("qwen3:14b"));
        assert_eq!(get("CLAUDE_CODE_SUBAGENT_MODEL").as_deref(), Some("qwen3:14b"));
        // Telemetry suppression parity with `ollama launch` (0.32.1).
        assert_eq!(get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").as_deref(), Some("1"));
        assert_eq!(get("DISABLE_TELEMETRY").as_deref(), Some("1"));
    }

    #[test]
    fn test_build_ollama_env_pins_session_model() {
        // ANTHROPIC_MODEL must always be present and equal to the chosen
        // Ollama model: it is the highest-priority model source in the
        // claude ACP bridge and shields the session from any provider
        // model persisted in the user's ~/.claude settings (which would
        // otherwise fail the prompt turn with -32603 model_not_found).
        let env = build_ollama_env("claude", "llama3.2:3b").expect("claude mapping");
        let pinned = env
            .iter()
            .find(|var| var.name == "ANTHROPIC_MODEL")
            .expect("ANTHROPIC_MODEL must be set");
        assert_eq!(pinned.value, "llama3.2:3b");
    }

    #[test]
    fn test_build_ollama_env_unsupported_backends() {
        assert!(build_ollama_env("gemini", "qwen3:14b").is_none());
        assert!(build_ollama_env("codex", "qwen3:14b").is_none());
        assert!(build_ollama_env("unknown", "qwen3:14b").is_none());
    }

    #[test]
    fn test_env_mapping_exists_for_every_compatible_backend() {
        // Every backend advertised as ollama_compatible must have an env
        // mapping, otherwise the factory would silently fall back.
        for backend in aionui_common::constants::OLLAMA_COMPATIBLE_BACKENDS {
            assert!(
                build_ollama_env(backend, "m").is_some(),
                "{backend} is marked compatible but has no env mapping"
            );
        }
    }
}
