/// Static asset-domain error used below the HTTP boundary.
#[derive(Debug)]
pub enum AssetError {
    NotFound(String),
    Forbidden(String),
    Internal(String),
}

impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(message) => write!(f, "Not found: {message}"),
            Self::Forbidden(message) => write!(f, "Forbidden: {message}"),
            Self::Internal(message) => write!(f, "Internal error: {message}"),
        }
    }
}

impl std::error::Error for AssetError {}
