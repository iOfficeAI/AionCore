//! Backend capability values shared across domain ports.

/// MCP transports a backend has been verified to configure and use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpTransportCapabilities {
    pub stdio: bool,
    pub sse: bool,
    pub streamable_http: bool,
}

/// Evidence source used to resolve an effective backend capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CapabilityOrigin {
    DirectDescriptor,
    InternalDescriptor,
    AcpHandshake,
    #[default]
    Unknown,
}

impl CapabilityOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirectDescriptor => "direct_descriptor",
            Self::InternalDescriptor => "internal_descriptor",
            Self::AcpHandshake => "acp_handshake",
            Self::Unknown => "unknown",
        }
    }
}

/// Unified capability conclusion consumed by Team without knowing any vendor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResolvedBackendCapabilities {
    pub mcp: McpTransportCapabilities,
    pub cli_fallback: bool,
    pub origin: CapabilityOrigin,
}
