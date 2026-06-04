/// MCP crate-level errors.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP server not found: {0}")]
    NotFound(String),

    #[error("MCP server name conflict: {0}")]
    Conflict(String),

    #[error("Invalid MCP server edit: {0}")]
    InvalidEdit(String),

    #[error("Invalid transport configuration: {0}")]
    InvalidTransport(String),

    #[error("Agent CLI not installed: {0}")]
    AgentNotInstalled(String),

    #[error("Agent operation failed: {0}")]
    AgentOperationFailed(String),

    #[error("Connection test failed: {0}")]
    ConnectionFailed(String),

    #[error("OAuth error: {0}")]
    OAuth(String),

    #[error("{0}")]
    Database(#[from] aionui_db::DbError),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(
            McpError::NotFound("mcp_1".into()).to_string(),
            "MCP server not found: mcp_1"
        );
        assert_eq!(
            McpError::InvalidTransport("bad".into()).to_string(),
            "Invalid transport configuration: bad"
        );
        assert_eq!(
            McpError::InvalidEdit("rename forbidden".into()).to_string(),
            "Invalid MCP server edit: rename forbidden"
        );
    }
}
