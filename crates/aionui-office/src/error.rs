#[derive(Debug, thiserror::Error)]
pub enum OfficeError {
    #[error("officecli not found")]
    OfficecliNotFound,

    #[error("officecli install failed: {0}")]
    InstallFailed(String),

    #[error("preview start failed: {0}")]
    StartFailed(String),

    #[error("port readiness timeout for {0}")]
    PortTimeout(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("snapshot error: {0}")]
    Snapshot(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("conversion error: {0}")]
    Conversion(String),

    #[error("external tool not found: {0}")]
    ToolNotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(OfficeError::OfficecliNotFound.to_string(), "officecli not found");
        assert_eq!(
            OfficeError::InstallFailed("installer error".into()).to_string(),
            "officecli install failed: installer error"
        );
        assert_eq!(
            OfficeError::PortTimeout("/a.docx".into()).to_string(),
            "port readiness timeout for /a.docx"
        );
        assert_eq!(
            OfficeError::Conversion("bad data".into()).to_string(),
            "conversion error: bad data"
        );
        assert_eq!(
            OfficeError::ToolNotFound("pandoc".into()).to_string(),
            "external tool not found: pandoc"
        );
    }
}
