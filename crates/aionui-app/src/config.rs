//! Application configuration parsed from CLI arguments + key derivation.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMode {
    Local,
    WebUi,
    AionPro,
}

impl IdentityMode {
    pub fn auth_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::WebUi => "webui",
            Self::AionPro => "aionpro",
        }
    }

    pub fn is_local(self) -> bool {
        self == Self::Local
    }
}

/// Application configuration parsed from CLI arguments.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
    pub app_version: String,
    /// Run in local embedded mode (skip authentication, use system_default_user).
    pub local: bool,
    pub identity_mode: IdentityMode,
    pub bootstrap_secret: Option<String>,
    /// Optional operator-configured workspace exposed only to the seeded
    /// `system_default_user` in authenticated user-session modes.
    pub bootstrap_workspace: Option<PathBuf>,
    /// Enable one-time initial administrator provisioning. The process-level
    /// WebUI bootstrap enables this explicitly; library/test defaults do not
    /// perform credential filesystem side effects.
    pub bootstrap_initial_admin: bool,
    /// Per-launch capability shared only with the packaged local client.
    /// Required in Local mode; never accepted in a URL or cookie.
    pub local_client_secret: Option<String>,
    /// Exact browser origins allowed to reach Core cross-origin. WebUI mode
    /// remains same-origin; Local additionally permits the packaged `null`
    /// origin at router assembly time.
    pub allowed_origins: Vec<String>,
    /// Dump prompt diagnostics under `data_dir/prompt-dumps`.
    pub dump_prompts: bool,
    /// Explicitly authorize backup and rebuild for corruption-like local databases.
    pub recover_corrupted_database: bool,
}

impl AppConfig {
    pub fn effective_identity_mode(&self) -> IdentityMode {
        if self.local {
            IdentityMode::Local
        } else {
            self.identity_mode
        }
    }

    /// Format as `host:port` for socket binding.
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Local URL helpers should use to call this backend from the same machine.
    pub fn local_base_url(&self) -> String {
        let host = match self.host.as_str() {
            "0.0.0.0" | "::" => "127.0.0.1",
            other => other,
        };
        format!("http://{host}:{}", self.port)
    }

    /// Path to the SQLite database file.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("aionui-backend.db")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: aionui_common::constants::DEFAULT_HOST.to_string(),
            port: aionui_common::constants::DEFAULT_PORT,
            data_dir: PathBuf::from("data"),
            work_dir: PathBuf::from("data"),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            local: false,
            identity_mode: IdentityMode::WebUi,
            bootstrap_secret: None,
            bootstrap_workspace: None,
            bootstrap_initial_admin: false,
            local_client_secret: None,
            allowed_origins: Vec::new(),
            dump_prompts: false,
            recover_corrupted_database: false,
        }
    }
}

/// Parse and normalize `AIONCORE_ALLOWED_ORIGINS`.
///
/// Entries are comma-separated exact HTTP(S) origins, or the literal `null`
/// for packaged/native webviews. Paths, credentials, query strings,
/// fragments, wildcards, and opaque schemes are rejected.
pub fn parse_allowed_origins(raw: Option<&str>) -> Result<Vec<String>, String> {
    let mut origins = Vec::new();
    for entry in raw.unwrap_or_default().split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let normalized = if entry == "null" {
            "null".to_string()
        } else {
            if entry == "*" {
                return Err("wildcard origins are not allowed".to_string());
            }
            let parsed = url::Url::parse(entry).map_err(|error| format!("invalid origin '{entry}': {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(format!("origin '{entry}' must use http or https"));
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(format!("origin '{entry}' must not contain credentials"));
            }
            if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
                return Err(format!("origin '{entry}' must not contain a path, query, or fragment"));
            }
            let origin = parsed.origin().ascii_serialization();
            if origin == "null" {
                return Err(format!("origin '{entry}' is not a tuple origin"));
            }
            origin
        };
        if !origins.contains(&normalized) {
            origins.push(normalized);
        }
    }
    Ok(origins)
}

/// Validate the per-launch Local client capability.
///
/// The launcher supplies 32 random bytes as unpadded Base64URL (43 ASCII
/// characters), which is safe in both an HTTP header value and a WebSocket
/// subprotocol token.
pub fn validate_local_client_secret(secret: &str) -> Result<(), String> {
    if secret.len() != 43 {
        return Err("local client secret must encode exactly 32 random bytes as unpadded Base64URL".to_string());
    }
    if !secret
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("local client secret must contain only unpadded Base64URL characters".to_string());
    }
    Ok(())
}

/// Derive a 32-byte encryption key from the JWT secret using SHA-256.
pub fn derive_encryption_key(jwt_secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"aionui-encryption-key:");
    hasher.update(jwt_secret.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 25808);
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(config.identity_mode, IdentityMode::WebUi);
        assert!(config.bootstrap_secret.is_none());
        assert!(config.bootstrap_workspace.is_none());
        assert!(!config.dump_prompts);
        assert!(!config.recover_corrupted_database);
    }

    #[test]
    fn test_app_config_socket_addr() {
        let config = AppConfig {
            host: "0.0.0.0".to_string(),
            port: 3000,
            ..Default::default()
        };
        assert_eq!(config.socket_addr(), "0.0.0.0:3000");
    }

    #[test]
    fn local_base_url_uses_loopback_for_wildcard_host() {
        let config = AppConfig {
            host: "0.0.0.0".to_string(),
            port: 49152,
            ..Default::default()
        };
        assert_eq!(config.local_base_url(), "http://127.0.0.1:49152");
    }

    #[test]
    fn test_app_config_database_path() {
        let config = AppConfig {
            data_dir: PathBuf::from("/tmp/aionui"),
            ..Default::default()
        };
        assert_eq!(config.database_path(), PathBuf::from("/tmp/aionui/aionui-backend.db"));
    }

    #[test]
    fn allowed_origins_are_normalized_and_deduplicated() {
        assert_eq!(
            parse_allowed_origins(Some(" https://EXAMPLE.com/,null,https://example.com ")).unwrap(),
            vec!["https://example.com", "null"]
        );
    }

    #[test]
    fn allowed_origins_reject_wildcards_and_non_origin_urls() {
        for value in [
            "*",
            "file:///tmp/index.html",
            "https://user@example.com",
            "https://example.com/path",
            "https://example.com?query=1",
        ] {
            assert!(parse_allowed_origins(Some(value)).is_err(), "{value} must be rejected");
        }
    }

    #[test]
    fn local_client_secret_requires_32_bytes_of_unpadded_base64url() {
        assert!(validate_local_client_secret("abcdefghijklmnopqrstuvwxyzABCDEFGH012345678").is_ok());
        assert!(validate_local_client_secret("too-short").is_err());
        assert!(validate_local_client_secret("abcdefghijklmnopqrstuvwxyzABCDEFGH01234567=").is_err());
    }
}
