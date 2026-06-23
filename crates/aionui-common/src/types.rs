use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// EnvVar / CommandSpec
// ---------------------------------------------------------------------------

/// A name=value environment variable pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// A command with its arguments and environment variables.
///
/// This is the common building block shared by CLI agent spawning,
/// MCP server transports, and agent discovery types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvVar>,
    pub cwd: Option<String>,
}

// ---------------------------------------------------------------------------
// UpdateType
// ---------------------------------------------------------------------------

/// Type of available update based on semver comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateType {
    Major,
    Minor,
    Patch,
}

/// Model selection config — references a provider and a specific model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderWithModel {
    pub provider_id: String,
    pub model: String,
    pub use_model: Option<String>,
}

/// A pending tool-call confirmation item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confirmation {
    pub id: String,
    pub call_id: String,
    pub title: Option<String>,
    pub action: Option<String>,
    pub description: String,
    pub command_type: Option<String>,
    pub options: Vec<ConfirmationOption>,
}

/// A single option within a confirmation dialog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmationOption {
    pub label: String,
    pub value: serde_json::Value,
    pub params: Option<HashMap<String, String>>,
}

/// Semantic version info with comparison and update detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub current: String,
    pub latest: String,
    pub minimum_required: Option<String>,
    pub release_notes: Option<String>,
}

impl VersionInfo {
    /// Check if an update is available (latest > current).
    pub fn is_update_available(&self) -> bool {
        semver::Version::parse(&self.latest)
            .ok()
            .zip(semver::Version::parse(&self.current).ok())
            .is_some_and(|(latest, current)| latest > current)
    }

    /// Check if the update is forced (current < minimum_required).
    pub fn is_forced(&self) -> bool {
        self.minimum_required
            .as_ref()
            .and_then(|min| {
                semver::Version::parse(min)
                    .ok()
                    .zip(semver::Version::parse(&self.current).ok())
                    .map(|(min_ver, cur_ver)| cur_ver < min_ver)
            })
            .unwrap_or(false)
    }

    /// Determine the update type (major, minor, or patch) based on semver diff.
    /// Returns `None` if no update is available or versions are unparseable.
    pub fn get_update_type(&self) -> Option<UpdateType> {
        let latest = semver::Version::parse(&self.latest).ok()?;
        let current = semver::Version::parse(&self.current).ok()?;
        if latest <= current {
            return None;
        }
        if latest.major > current.major {
            Some(UpdateType::Major)
        } else if latest.minor > current.minor {
            Some(UpdateType::Minor)
        } else {
            Some(UpdateType::Patch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_with_model_serde() {
        let p = ProviderWithModel {
            provider_id: "openai-1".into(),
            model: "gpt-4".into(),
            use_model: Some("gpt-4-turbo".into()),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["provider_id"], "openai-1");
        assert_eq!(json["model"], "gpt-4");
        assert_eq!(json["use_model"], "gpt-4-turbo");
    }

    #[test]
    fn test_confirmation_serde() {
        let c = Confirmation {
            id: "c1".into(),
            call_id: "call1".into(),
            title: Some("Run command?".into()),
            action: None,
            description: "Execute shell command".into(),
            command_type: Some("bash".into()),
            options: vec![ConfirmationOption {
                label: "Allow".into(),
                value: serde_json::json!(true),
                params: None,
            }],
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["call_id"], "call1");
        assert_eq!(json["command_type"], "bash");
    }

    #[test]
    fn test_version_info_update_available() {
        let v = VersionInfo {
            current: "1.0.0".into(),
            latest: "1.1.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert!(v.is_update_available());
        assert!(!v.is_forced());
    }

    #[test]
    fn test_version_info_no_update() {
        let v = VersionInfo {
            current: "2.0.0".into(),
            latest: "1.5.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert!(!v.is_update_available());
    }

    #[test]
    fn test_version_info_forced_update() {
        let v = VersionInfo {
            current: "1.0.0".into(),
            latest: "2.0.0".into(),
            minimum_required: Some("1.5.0".into()),
            release_notes: Some("Critical fix".into()),
        };
        assert!(v.is_update_available());
        assert!(v.is_forced());
    }

    #[test]
    fn test_version_info_not_forced() {
        let v = VersionInfo {
            current: "1.5.0".into(),
            latest: "2.0.0".into(),
            minimum_required: Some("1.2.0".into()),
            release_notes: None,
        };
        assert!(v.is_update_available());
        assert!(!v.is_forced());
    }

    #[test]
    fn test_version_info_invalid_semver() {
        let v = VersionInfo {
            current: "not-a-version".into(),
            latest: "1.0.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert!(!v.is_update_available());
        assert_eq!(v.get_update_type(), None);
    }

    #[test]
    fn test_version_info_get_update_type_major() {
        let v = VersionInfo {
            current: "1.2.3".into(),
            latest: "2.0.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert_eq!(v.get_update_type(), Some(UpdateType::Major));
    }

    #[test]
    fn test_version_info_get_update_type_minor() {
        let v = VersionInfo {
            current: "1.2.3".into(),
            latest: "1.5.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert_eq!(v.get_update_type(), Some(UpdateType::Minor));
    }

    #[test]
    fn test_version_info_get_update_type_patch() {
        let v = VersionInfo {
            current: "1.2.3".into(),
            latest: "1.2.5".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert_eq!(v.get_update_type(), Some(UpdateType::Patch));
    }

    #[test]
    fn test_version_info_get_update_type_none_when_same() {
        let v = VersionInfo {
            current: "1.0.0".into(),
            latest: "1.0.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert_eq!(v.get_update_type(), None);
    }

    #[test]
    fn test_version_info_get_update_type_none_when_older() {
        let v = VersionInfo {
            current: "2.0.0".into(),
            latest: "1.5.0".into(),
            minimum_required: None,
            release_notes: None,
        };
        assert_eq!(v.get_update_type(), None);
    }
}
