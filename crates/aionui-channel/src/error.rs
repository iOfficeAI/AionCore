/// Channel crate-level errors.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("Plugin not found: {0}")]
    PluginNotFound(String),

    #[error("Invalid plugin type: {0}")]
    InvalidPluginType(String),

    #[error("Plugin already running: {0}")]
    PluginAlreadyRunning(String),

    #[error("Invalid plugin configuration: {0}")]
    InvalidConfig(String),

    #[error("Plugin connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Pairing code not found: {0}")]
    PairingNotFound(String),

    #[error("Pairing code expired: {0}")]
    PairingExpired(String),

    #[error("Pairing code already processed: {0}")]
    PairingAlreadyProcessed(String),

    #[error("User not found: {0}")]
    UserNotFound(String),

    #[error("User not authorized: {0}")]
    UserNotAuthorized(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Credential encryption failed: {0}")]
    EncryptionFailed(String),

    #[error("Credential decryption failed: {0}")]
    DecryptionFailed(String),

    #[error("Platform API error: {0}")]
    PlatformApi(String),

    #[error("Message send failed: {0}")]
    MessageSendFailed(String),

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
            ChannelError::PluginNotFound("tg".into()).to_string(),
            "Plugin not found: tg"
        );
        assert_eq!(
            ChannelError::PairingExpired("123456".into()).to_string(),
            "Pairing code expired: 123456"
        );
        assert_eq!(
            ChannelError::InvalidConfig("bad".into()).to_string(),
            "Invalid plugin configuration: bad"
        );
    }
}
