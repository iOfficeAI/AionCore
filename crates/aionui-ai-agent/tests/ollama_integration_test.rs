//! Integration tests for the Ollama integration.
//!
//! These tests exercise the public API of the ollama module
//! and the OLLAMA_COMPATIBLE_BACKENDS constant from aionui-common.

use aionui_ai_agent::ollama::build_ollama_env;
use aionui_api_types::AcpBuildExtra;
use aionui_common::constants::{OLLAMA_COMPATIBLE_BACKENDS, OLLAMA_DEFAULT_BASE_URL, is_ollama_supported_agent};

#[test]
fn compatible_backends_have_env_mappings() {
    // Every backend advertised as ollama_compatible must produce a
    // non-empty env mapping, otherwise the factory silently falls back.
    for backend in OLLAMA_COMPATIBLE_BACKENDS {
        assert!(is_ollama_supported_agent(backend));
        let env = build_ollama_env(backend, "qwen3:14b").expect("compatible backend must have an env mapping");
        assert!(!env.is_empty());
    }
}

#[test]
fn claude_env_routes_to_local_ollama() {
    let env = build_ollama_env("claude", "llama3.2").expect("claude mapping");
    let get = |name: &str| env.iter().find(|var| var.name == name).map(|var| var.value.as_str());

    // Mirrors what `ollama launch claude` injects (captured on 0.32.1)
    // plus ANTHROPIC_MODEL, required headless (see build_ollama_env docs).
    assert_eq!(get("ANTHROPIC_BASE_URL"), Some(OLLAMA_DEFAULT_BASE_URL));
    assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("ollama"));
    assert_eq!(get("ANTHROPIC_API_KEY"), Some(""));
    assert_eq!(get("ANTHROPIC_MODEL"), Some("llama3.2"));
    assert_eq!(get("ANTHROPIC_DEFAULT_OPUS_MODEL"), Some("llama3.2"));
    assert_eq!(get("ANTHROPIC_DEFAULT_SONNET_MODEL"), Some("llama3.2"));
    assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some("llama3.2"));
    assert_eq!(get("CLAUDE_CODE_SUBAGENT_MODEL"), Some("llama3.2"));
    assert_eq!(get("CLAUDE_CODE_ATTRIBUTION_HEADER"), Some("0"));
    assert_eq!(get("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY"), Some("1"));
    assert_eq!(get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"), Some("1"));
    assert_eq!(get("DISABLE_ERROR_REPORTING"), Some("1"));
    assert_eq!(get("DISABLE_FEEDBACK_COMMAND"), Some("1"));
    assert_eq!(get("DISABLE_TELEMETRY"), Some("1"));
}

#[test]
fn qwen_env_routes_to_local_ollama() {
    let env = build_ollama_env("qwen", "llama3.2").expect("qwen mapping");
    let get = |name: &str| env.iter().find(|var| var.name == name).map(|var| var.value.as_str());

    // Mirrors what `ollama launch qwen` injects (captured on 0.32.1);
    // qwen-code speaks the OpenAI wire protocol, hence the /v1 endpoint.
    assert_eq!(get("OPENAI_API_KEY"), Some("ollama"));
    assert_eq!(get("OPENAI_BASE_URL"), Some("http://127.0.0.1:11434/v1"));
    assert_eq!(get("OPENAI_MODEL"), Some("llama3.2"));
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
        // `ollama launch` supports these interactively, but AionCore has
        // no verified headless env mapping for them yet.
        "opencode",
        "codex",
        "copilot",
        "pi",
        "hermes",
        "droid",
    ];
    for agent in unsupported {
        assert!(!is_ollama_supported_agent(agent));
        assert!(build_ollama_env(agent, "qwen3:14b").is_none());
    }
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
    let extra: AcpBuildExtra = serde_json::from_str(r#"{"backend":"claude","use_ollama":true}"#).unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model, None);
}

#[test]
fn acp_build_extra_ollama_model_deserializes() {
    // When the frontend supplies ollama_model, it must be preserved.
    let extra: AcpBuildExtra =
        serde_json::from_str(r#"{"backend":"claude","use_ollama":true,"ollama_model":"llama3.2"}"#).unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("llama3.2"));
}

#[test]
fn acp_build_extra_ollama_model_with_tagged_model() {
    // Tagged models like "qwen3:14b" should be preserved verbatim.
    let extra: AcpBuildExtra =
        serde_json::from_str(r#"{"backend":"claude","use_ollama":true,"ollama_model":"qwen3:14b"}"#).unwrap();
    assert!(extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("qwen3:14b"));
}

#[test]
fn acp_build_extra_ollama_model_without_use_ollama() {
    // Sending ollama_model without use_ollama should parse but
    // effectively be ignored at runtime (native launch path).
    let extra: AcpBuildExtra = serde_json::from_str(r#"{"backend":"claude","ollama_model":"llama3.2"}"#).unwrap();
    assert!(!extra.use_ollama);
    assert_eq!(extra.ollama_model.as_deref(), Some("llama3.2"));
}
