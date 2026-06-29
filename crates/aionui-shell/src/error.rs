#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error("directory not found: {0}")]
    DirectoryNotFound(String),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("tool not installed: {0}")]
    ToolNotInstalled(String),

    #[error("command failed: {0}")]
    CommandFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("STT is not enabled")]
    Disabled,

    #[error("OpenAI STT is not configured: missing API key")]
    OpenaiNotConfigured,

    #[error("Deepgram STT is not configured: missing API key")]
    DeepgramNotConfigured,

    #[error("STT request failed: {0}")]
    RequestFailed(String),

    #[error("STT unknown error: {0}")]
    Unknown(String),

    #[error("STT streaming is not supported for this model or endpoint")]
    StreamUnsupported,

    #[error("STT stream protocol error: {0}")]
    StreamProtocol(String),
}

impl SttError {
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Disabled => "STT_DISABLED",
            Self::OpenaiNotConfigured => "STT_OPENAI_NOT_CONFIGURED",
            Self::DeepgramNotConfigured => "STT_DEEPGRAM_NOT_CONFIGURED",
            Self::RequestFailed(_) => "STT_REQUEST_FAILED",
            Self::Unknown(_) => "STT_UNKNOWN",
            Self::StreamUnsupported => "STT_STREAM_UNSUPPORTED",
            Self::StreamProtocol(_) => "STT_STREAM_PROTOCOL",
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::Disabled | Self::OpenaiNotConfigured | Self::DeepgramNotConfigured => 400,
            Self::RequestFailed(_) => 502,
            Self::Unknown(_) => 500,
            Self::StreamUnsupported | Self::StreamProtocol(_) => 400,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_error_display_messages() {
        assert_eq!(
            ShellError::FileNotFound("/a.txt".into()).to_string(),
            "file not found: /a.txt"
        );
        assert_eq!(
            ShellError::DirectoryNotFound("/dir".into()).to_string(),
            "directory not found: /dir"
        );
        assert_eq!(ShellError::InvalidUrl("bad".into()).to_string(), "invalid URL: bad");
        assert_eq!(
            ShellError::ToolNotInstalled("code".into()).to_string(),
            "tool not installed: code"
        );
        assert_eq!(
            ShellError::CommandFailed("oops".into()).to_string(),
            "command failed: oops"
        );
    }

    #[test]
    fn stt_error_codes() {
        assert_eq!(SttError::Disabled.error_code(), "STT_DISABLED");
        assert_eq!(SttError::OpenaiNotConfigured.error_code(), "STT_OPENAI_NOT_CONFIGURED");
        assert_eq!(
            SttError::DeepgramNotConfigured.error_code(),
            "STT_DEEPGRAM_NOT_CONFIGURED"
        );
        assert_eq!(SttError::RequestFailed("x".into()).error_code(), "STT_REQUEST_FAILED");
        assert_eq!(SttError::Unknown("x".into()).error_code(), "STT_UNKNOWN");
        assert_eq!(SttError::StreamUnsupported.error_code(), "STT_STREAM_UNSUPPORTED");
        assert_eq!(
            SttError::StreamProtocol("bad frame".into()).error_code(),
            "STT_STREAM_PROTOCOL"
        );
    }

    #[test]
    fn stt_status_codes() {
        assert_eq!(SttError::Disabled.status_code(), 400);
        assert_eq!(SttError::OpenaiNotConfigured.status_code(), 400);
        assert_eq!(SttError::DeepgramNotConfigured.status_code(), 400);
        assert_eq!(SttError::RequestFailed("x".into()).status_code(), 502);
        assert_eq!(SttError::Unknown("x".into()).status_code(), 500);
        assert_eq!(SttError::StreamUnsupported.status_code(), 400);
        assert_eq!(SttError::StreamProtocol("x".into()).status_code(), 400);
    }

    #[test]
    fn stt_error_display_messages() {
        assert_eq!(SttError::Disabled.to_string(), "STT is not enabled");
        assert_eq!(
            SttError::OpenaiNotConfigured.to_string(),
            "OpenAI STT is not configured: missing API key"
        );
        assert_eq!(
            SttError::DeepgramNotConfigured.to_string(),
            "Deepgram STT is not configured: missing API key"
        );
        assert_eq!(
            SttError::RequestFailed("timeout".into()).to_string(),
            "STT request failed: timeout"
        );
        assert_eq!(SttError::Unknown("oops".into()).to_string(), "STT unknown error: oops");
        assert_eq!(
            SttError::StreamUnsupported.to_string(),
            "STT streaming is not supported for this model or endpoint"
        );
        assert_eq!(
            SttError::StreamProtocol("unexpected frame".into()).to_string(),
            "STT stream protocol error: unexpected frame"
        );
    }
}
