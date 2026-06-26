/// Extension system domain errors.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionError {
    #[error("Manifest validation failed: {0}")]
    ManifestValidation(String),

    #[error("Extension name '{name}' uses reserved prefix '{prefix}'")]
    ReservedNamePrefix { name: String, prefix: String },

    #[error("Invalid version '{version}': {reason}")]
    InvalidVersion { version: String, reason: String },

    #[error("Undefined environment variable: {0}")]
    UndefinedEnvVariable(String),

    #[error("File reference not found: {0}")]
    FileReferenceNotFound(String),

    #[error("Path traversal detected: {0}")]
    PathTraversal(String),

    #[error("Engine incompatible: extension '{name}' requires aionui {required}, got {actual}")]
    EngineIncompatible {
        name: String,
        required: String,
        actual: String,
    },

    #[error("API version incompatible: extension '{name}' requires API {required}, supported {supported}")]
    ApiVersionIncompatible {
        name: String,
        required: String,
        supported: String,
    },

    #[error("WebUI route '{route}' must be under '/{extension_name}/' namespace")]
    InvalidWebuiRouteNamespace { extension_name: String, route: String },

    #[error("WebUI route '{route}' uses reserved prefix '{prefix}'")]
    ReservedWebuiRoute { route: String, prefix: String },

    #[error("Theme CSS file not found: {0}")]
    ThemeCssNotFound(String),

    #[error("Contribution resolution failed for '{extension_name}': {reason}")]
    ResolutionFailed { extension_name: String, reason: String },

    #[error("Lifecycle hook '{hook}' timed out after {timeout_secs}s for extension '{extension_name}'")]
    HookTimeout {
        extension_name: String,
        hook: String,
        timeout_secs: u64,
    },

    #[error("Lifecycle hook '{hook}' failed for extension '{extension_name}': {reason}")]
    HookFailed {
        extension_name: String,
        hook: String,
        reason: String,
    },

    #[error("Lifecycle hook script not found: {0}")]
    HookNotFound(String),

    #[error("Extension not found: {0}")]
    NotFound(String),

    #[error("State persistence failed: {0}")]
    StatePersistence(String),

    #[error("Cannot delete built-in skill: {0}")]
    BuiltinSkillDeletion(String),

    #[error("Skill not found: {0}")]
    SkillNotFound(String),

    #[error("Invalid skill path: {0}")]
    InvalidSkillPath(String),

    #[error("Skill frontmatter is invalid: {0}")]
    SkillInvalidFrontmatter(String),

    #[error("No skill directories found: {0}")]
    SkillImportNoSkillFound(String),

    #[error("Invalid skill import source: {0}")]
    SkillImportInvalidSource(String),

    #[error("Skill import does not allow symlink entries: {0}")]
    SkillImportSymlinkEntry(String),

    #[error("Skill import file is too large: {file_bytes} bytes, limit {limit_bytes} bytes")]
    SkillImportFileTooLarge {
        file_path: Option<String>,
        file_bytes: u64,
        limit_bytes: u64,
    },

    #[error("Skill import is too large: {total_bytes} bytes, limit {limit_bytes} bytes")]
    SkillImportTotalTooLarge { total_bytes: u64, limit_bytes: u64 },

    #[error("Invalid skill zip archive: {0}")]
    SkillImportInvalidZip(String),

    #[error("{0}")]
    Db(#[from] aionui_db::DbError),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Internal extension error: {0}")]
    Internal(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    JsonParse(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_validation_error_display() {
        let err = ExtensionError::ManifestValidation("name is required".into());
        assert_eq!(err.to_string(), "Manifest validation failed: name is required");
    }

    #[test]
    fn test_reserved_name_prefix_error_display() {
        let err = ExtensionError::ReservedNamePrefix {
            name: "aion-test".into(),
            prefix: "aion-".into(),
        };
        assert_eq!(
            err.to_string(),
            "Extension name 'aion-test' uses reserved prefix 'aion-'"
        );
    }

    #[test]
    fn test_invalid_version_error_display() {
        let err = ExtensionError::InvalidVersion {
            version: "not-semver".into(),
            reason: "unexpected character".into(),
        };
        assert_eq!(err.to_string(), "Invalid version 'not-semver': unexpected character");
    }

    #[test]
    fn test_undefined_env_variable_error_display() {
        let err = ExtensionError::UndefinedEnvVariable("MY_SECRET".into());
        assert_eq!(err.to_string(), "Undefined environment variable: MY_SECRET");
    }

    #[test]
    fn test_file_reference_not_found_error_display() {
        let err = ExtensionError::FileReferenceNotFound("prompts/system.md".into());
        assert_eq!(err.to_string(), "File reference not found: prompts/system.md");
    }

    #[test]
    fn test_path_traversal_error_display() {
        let err = ExtensionError::PathTraversal("../../etc/passwd".into());
        assert_eq!(err.to_string(), "Path traversal detected: ../../etc/passwd");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err = ExtensionError::from(io_err);
        assert!(matches!(err, ExtensionError::Io(_)));
    }
}
