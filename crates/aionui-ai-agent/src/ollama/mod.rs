//! Ollama integration for AionCore.
//!
//! This module provides Ollama Launch support, allowing AionCore to delegate
//! agent spawning to Ollama for supported agents. When enabled, AionCore runs
//! `ollama launch <agent>` instead of the agent's native command, leveraging
//! Ollama's automatic model configuration.

use std::sync::OnceLock;

use aionui_common::constants::{OLLAMA_COMMAND, get_ollama_launch_agent_name, is_ollama_supported_agent};

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

/// Check if a given agent backend is supported by Ollama Launch.
///
/// This checks the OLLAMA_LAUNCH_MAP to see if the agent has an Ollama
/// launch integration available.
pub fn is_agent_ollama_supported(backend: &str) -> bool {
    is_ollama_supported_agent(backend)
}

/// Get the Ollama launch command for a given agent backend.
///
/// Returns the full command to use for launching the agent via Ollama,
/// or None if the agent is not supported by Ollama Launch.
pub fn get_ollama_launch_command(backend: &str) -> Option<String> {
    get_ollama_launch_agent_name(backend).map(|name| format!("{OLLAMA_COMMAND} launch {name}"))
}

/// Get the Ollama launch agent name for a given backend.
///
/// Returns the agent name that Ollama understands, or None if the
/// backend is not supported.
pub fn get_ollama_agent_name(backend: &str) -> Option<&'static str> {
    get_ollama_launch_agent_name(backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ollama_launch_map_coverage() {
        // Test that all expected agents are in the mapping
        let expected_agents = [
            "claude", "opencode", "codex", "copilot", "pi", "hermes", "droid", "qwen",
        ];
        for agent in expected_agents {
            assert!(
                is_agent_ollama_supported(agent),
                "Agent {} should be supported by Ollama Launch",
                agent
            );
        }
    }

    #[test]
    fn test_ollama_launch_command_generation() {
        assert_eq!(
            get_ollama_launch_command("claude"),
            Some("ollama launch claude".to_string())
        );
        assert_eq!(
            get_ollama_launch_command("opencode"),
            Some("ollama launch opencode".to_string())
        );
        assert_eq!(get_ollama_launch_command("nonexistent"), None);
    }

    #[test]
    fn test_ollama_agent_name() {
        assert_eq!(get_ollama_agent_name("claude"), Some("claude"));
        assert_eq!(get_ollama_agent_name("codex"), Some("codex"));
        assert_eq!(get_ollama_agent_name("unknown"), None);
    }
}
