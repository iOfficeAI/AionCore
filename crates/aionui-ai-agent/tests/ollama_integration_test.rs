//! Integration tests for Ollama Launch functionality.
//!
//! These tests exercise the public API of the ollama module
//! and the OLLAMA_LAUNCH_MAP constant from aionui-common.

use aionui_ai_agent::ollama::{
    get_ollama_agent_name, get_ollama_launch_command, is_agent_ollama_supported, is_ollama_available,
};
use aionui_api_types::AcpBuildExtra;
use aionui_common::constants::{OLLAMA_COMMAND, get_ollama_launch_agent_name, is_ollama_supported_agent};

#[test]
fn supported_agents_have_consistent_names() {
    // Every supported agent should have a non-empty Ollama name
    let supported = [
        "claude", "opencode", "codex", "copilot", "pi", "hermes", "droid", "qwen",
    ];
    for agent in supported {
        assert!(is_agent_ollama_supported(agent));
        let name = get_ollama_agent_name(agent).expect("supported agent must have a name");
        assert!(!name.is_empty());
    }
}

#[test]
fn launch_command_contains_ollama_and_agent_name() {
    for agent in ["claude", "codex", "opencode"] {
        let cmd = get_ollama_launch_command(agent).expect("must produce a command");
        assert!(cmd.starts_with("ollama launch "));
        assert!(!cmd.ends_with(' '));
    }
}

#[test]
fn unsupported_agents_are_consistent() {
    let unsupported = [
        "gemini",
        "cursor",
        "auggie",
        "kimi",
        "goose",
        "codebuddy",
        "unknown_agent",
    ];
    for agent in unsupported {
        assert!(!is_agent_ollama_supported(agent));
        assert_eq!(get_ollama_agent_name(agent), None);
        assert_eq!(get_ollama_launch_command(agent), None);
    }
}

#[test]
fn module_delegates_to_common_constants() {
    // The ollama module functions should agree with the aionui-common
    // helper functions for every supported agent.
    for agent in [
        "claude", "opencode", "codex", "copilot", "pi", "hermes", "droid", "qwen",
    ] {
        assert_eq!(is_agent_ollama_supported(agent), is_ollama_supported_agent(agent));
        assert_eq!(get_ollama_agent_name(agent), get_ollama_launch_agent_name(agent));
    }
}

#[test]
fn ollama_constant_is_correct() {
    assert_eq!(OLLAMA_COMMAND, "ollama");
}

#[test]
fn is_ollama_available_does_not_panic() {
    // The function should return a bool without side effects.
    // We cannot assert a specific value because it depends on the
    // test environment, but we can verify it returns a consistent
    // result and doesn't panic.
    let first = is_ollama_available();
    let second = is_ollama_available();
    // OnceLock guarantees the same result every call
    assert_eq!(first, second);
}

#[test]
fn acp_build_extra_use_ollama_defaults_to_false() {
    // When use_ollama is absent from the JSON payload (as it will be
    // for all existing frontend versions), it must default to false.
    let extra: AcpBuildExtra = serde_json::from_str(r#"{"backend":"claude"}"#).unwrap();
    assert!(!extra.use_ollama);
}

#[test]
fn acp_build_extra_use_ollama_deserializes_true() {
    // When the frontend opts in, use_ollama must be true.
    let extra: AcpBuildExtra = serde_json::from_str(r#"{"backend":"claude","use_ollama":true}"#).unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.backend.as_deref(), Some("claude"));
}

#[test]
fn acp_build_extra_ollama_model_defaults_to_none() {
    // When ollama_model is absent from the JSON payload (all existing
    // frontend versions), it must default to None.
    let extra: AcpBuildExtra =
        serde_json::from_str(r#"{"backend":"claude","use_ollama":true}"#).unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model, None);
}

#[test]
fn acp_build_extra_ollama_model_deserializes() {
    // When the frontend supplies ollama_model, it must be preserved.
    let extra: AcpBuildExtra =
        serde_json::from_str(
            r#"{"backend":"claude","use_ollama":true,"ollama_model":"llama3.2"}"#,
        )
        .unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("llama3.2"));
}

#[test]
fn acp_build_extra_ollama_model_with_tagged_model() {
    // Tagged models like "qwen3:14b" should be preserved verbatim.
    let extra: AcpBuildExtra =
        serde_json::from_str(
            r#"{"backend":"opencode","use_ollama":true,"ollama_model":"qwen3:14b"}"#,
        )
        .unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("qwen3:14b"));
}

#[test]
fn acp_build_extra_ollama_model_without_use_ollama() {
    // Sending ollama_model without use_ollama should parse but
    // effectively be ignored at runtime (native launch path).
    let extra: AcpBuildExtra =
        serde_json::from_str(r#"{"backend":"claude","ollama_model":"llama3.2"}"#).unwrap();
    assert!(!extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("llama3.2"));
}
