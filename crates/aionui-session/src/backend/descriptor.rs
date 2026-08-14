//! Static capability descriptors for non-ACP backends.
//!
//! Direct CLI adapters cannot obtain an ACP initialize snapshot, so their MCP
//! capability comes from the adapter contract that performs the injection.

use aionui_common::{CapabilityOrigin, McpTransportCapabilities, ResolvedBackendCapabilities};

use super::cli_version::{VERIFIED_AGY_VERSION, VERIFIED_CLAUDE_VERSION, VERIFIED_CODEX_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCapabilities {
    pub image: bool,
    pub audio: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkCapabilities {
    pub at_turn: bool,
}

/// Constructed capability data. Vendor serialization remains in each adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCapabilityDescriptor {
    pub backend_id: &'static str,
    pub mcp: McpTransportCapabilities,
    pub cli_fallback: bool,
    pub verified_version: &'static str,
    pub origin: CapabilityOrigin,
    pub prompt: Option<PromptCapabilities>,
    pub fork: Option<ForkCapabilities>,
}

impl BackendCapabilityDescriptor {
    pub const fn resolved(self) -> ResolvedBackendCapabilities {
        ResolvedBackendCapabilities {
            mcp: self.mcp,
            cli_fallback: self.cli_fallback,
            origin: self.origin,
        }
    }
}

/// Return a descriptor only when the backend has constructed capability data.
pub fn backend_capability_descriptor(backend: &str) -> Option<BackendCapabilityDescriptor> {
    match backend {
        // verified: ~/.npm/_npx/ca6c9a6e3c4cc822/node_modules/
        // @agentclientprotocol/claude-agent-acp/dist/acp-agent.js:1872-1898
        "claude" => Some(BackendCapabilityDescriptor {
            backend_id: "claude",
            mcp: McpTransportCapabilities {
                stdio: true,
                sse: true,
                streamable_http: true,
            },
            cli_fallback: true,
            verified_version: VERIFIED_CLAUDE_VERSION,
            origin: CapabilityOrigin::DirectDescriptor,
            prompt: Some(PromptCapabilities {
                image: true,
                audio: false,
            }),
            fork: Some(ForkCapabilities { at_turn: false }),
        }),
        // verified: `codex mcp add --help` declares command-based stdio and
        // `--url` streamable HTTP, and does not declare SSE.
        "codex" => Some(BackendCapabilityDescriptor {
            backend_id: "codex",
            mcp: McpTransportCapabilities {
                stdio: true,
                sse: false,
                streamable_http: true,
            },
            cli_fallback: true,
            verified_version: VERIFIED_CODEX_VERSION,
            origin: CapabilityOrigin::DirectDescriptor,
            prompt: Some(PromptCapabilities {
                image: true,
                audio: false,
            }),
            fork: Some(ForkCapabilities { at_turn: true }),
        }),
        // verified: ~/.gemini/antigravity-cli/builtin/skills/
        // agy-customizations/docs/mcp_servers.md
        "antigravity" => Some(BackendCapabilityDescriptor {
            backend_id: "antigravity",
            mcp: McpTransportCapabilities {
                stdio: true,
                sse: true,
                streamable_http: false,
            },
            cli_fallback: true,
            verified_version: VERIFIED_AGY_VERSION,
            origin: CapabilityOrigin::DirectDescriptor,
            prompt: None,
            fork: None,
        }),
        "aionrs" => Some(BackendCapabilityDescriptor {
            backend_id: "aionrs",
            mcp: McpTransportCapabilities {
                stdio: true,
                sse: false,
                streamable_http: false,
            },
            cli_fallback: false,
            verified_version: env!("CARGO_PKG_VERSION"),
            origin: CapabilityOrigin::InternalDescriptor,
            prompt: None,
            fork: Some(ForkCapabilities { at_turn: true }),
        }),
        _ => None,
    }
}

/// Overlay constructed fields onto persisted discovery data. Constructed false
/// values are authoritative, so stale ACP history cannot re-enable a transport.
pub fn effective_agent_capabilities(backend: &str, persisted: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    let Some(descriptor) = backend_capability_descriptor(backend) else {
        return persisted.cloned();
    };
    let mut effective = persisted.cloned().unwrap_or_else(|| serde_json::json!({}));
    if !effective.is_object() {
        effective = serde_json::json!({});
    }
    effective["mcp_capabilities"] = serde_json::json!({
        "stdio": descriptor.mcp.stdio,
        "sse": descriptor.mcp.sse,
        "http": descriptor.mcp.streamable_http,
        "streamable_http": descriptor.mcp.streamable_http,
    });
    match descriptor.prompt {
        Some(prompt) => {
            effective["prompt_capabilities"] = serde_json::json!({
                "image": prompt.image,
                "audio": prompt.audio,
            });
        }
        None => {
            effective
                .as_object_mut()
                .expect("effective capability projection is normalized to an object")
                .remove("prompt_capabilities");
        }
    }
    match descriptor.fork {
        Some(fork) => {
            let effective_object = effective
                .as_object_mut()
                .expect("effective capability projection is normalized to an object");
            let session = effective_object
                .entry("session_capabilities")
                .or_insert_with(|| serde_json::json!({}));
            if !session.is_object() {
                *session = serde_json::json!({});
            }
            session["fork"] = serde_json::json!({
                "at_turn": fork.at_turn,
            });
        }
        None => {
            if let Some(session) = effective
                .get_mut("session_capabilities")
                .and_then(serde_json::Value::as_object_mut)
            {
                session.remove("fork");
            }
        }
    }
    Some(effective)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_descriptor_matrix_matches_verified_adapter_contracts() {
        let expected = [
            ("claude", true, true, true, VERIFIED_CLAUDE_VERSION),
            ("codex", true, false, true, VERIFIED_CODEX_VERSION),
            ("antigravity", true, true, false, VERIFIED_AGY_VERSION),
        ];
        for (backend, stdio, sse, http, version) in expected {
            let descriptor = backend_capability_descriptor(backend).expect("direct descriptor");
            assert_eq!(descriptor.origin, CapabilityOrigin::DirectDescriptor);
            assert_eq!(descriptor.mcp.stdio, stdio);
            assert_eq!(descriptor.mcp.sse, sse);
            assert_eq!(descriptor.mcp.streamable_http, http);
            assert!(descriptor.cli_fallback);
            assert_eq!(descriptor.verified_version, version);
        }
    }

    #[test]
    fn effective_projection_overrides_stale_direct_mcp_snapshot() {
        let historical = serde_json::json!({
            "mcp_capabilities": {"http": false, "sse": true},
            "session_capabilities": false,
            "unrelated": {"kept": true}
        });
        let effective = effective_agent_capabilities("codex", Some(&historical)).unwrap();
        assert_eq!(effective["mcp_capabilities"]["stdio"], true);
        assert_eq!(effective["mcp_capabilities"]["sse"], false);
        assert_eq!(effective["mcp_capabilities"]["http"], true);
        assert_eq!(effective["unrelated"]["kept"], true);
        assert_eq!(effective["session_capabilities"]["fork"]["at_turn"], true);
    }
}
