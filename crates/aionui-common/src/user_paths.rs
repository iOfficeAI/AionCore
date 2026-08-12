//! Per-user on-disk directory naming (multi-account filesystem isolation).
//!
//! The directory name is the Core user id with its `user_` prefix stripped:
//! readable, operable, and shorter than the full id for Windows path budget.
//! `system_default_user` has no prefix and is used verbatim.

use std::fmt;

use base64::Engine;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserDirNameError {
    Empty,
    Unsafe(String),
}

impl fmt::Display for UserDirNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserDirNameError::Empty => write!(f, "empty user id"),
            UserDirNameError::Unsafe(id) => write!(f, "user id is not filename-safe: {id}"),
        }
    }
}

impl std::error::Error for UserDirNameError {}

/// Directory name for a user's per-user on-disk root, e.g.
/// `user_019f8de8-…` -> `019f8de8-…`, `system_default_user` -> unchanged.
pub fn user_dir_name(user_id: &str) -> Result<String, UserDirNameError> {
    let trimmed = user_id.trim();
    if trimmed.is_empty() {
        return Err(UserDirNameError::Empty);
    }
    let name = trimmed.strip_prefix("user_").unwrap_or(trimmed);
    if name.is_empty() || !is_filename_safe(name) {
        return Err(UserDirNameError::Unsafe(user_id.to_owned()));
    }
    Ok(name.to_owned())
}

/// Return a bounded, filename-safe directory component for every user id.
///
/// Normal internal ids retain their readable representation. Unsafe legacy or
/// external ids use a deterministic SHA-256 fingerprint instead of ever being
/// joined into a filesystem path verbatim.
pub fn user_dir_name_or_fingerprint(user_id: &str) -> String {
    user_dir_name(user_id).unwrap_or_else(|_| {
        let digest = Sha256::digest(user_id.as_bytes());
        format!(
            "invalid-{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
        )
    })
}

fn is_filename_safe(name: &str) -> bool {
    if name == "." || name == ".." {
        return false;
    }
    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_user_prefix() {
        assert_eq!(
            user_dir_name("user_019f8de8-3537-7c73-8d92-3bfde17eb1ee").unwrap(),
            "019f8de8-3537-7c73-8d92-3bfde17eb1ee"
        );
    }

    #[test]
    fn default_user_is_verbatim() {
        assert_eq!(user_dir_name("system_default_user").unwrap(), "system_default_user");
    }

    #[test]
    fn rejects_empty_and_unsafe() {
        assert_eq!(user_dir_name(""), Err(UserDirNameError::Empty));
        assert!(matches!(user_dir_name("user_"), Err(UserDirNameError::Unsafe(_))));
        assert!(matches!(user_dir_name("user_a/b"), Err(UserDirNameError::Unsafe(_))));
        assert!(matches!(user_dir_name("user_.."), Err(UserDirNameError::Unsafe(_))));
        assert!(matches!(user_dir_name("../etc"), Err(UserDirNameError::Unsafe(_))));
    }

    #[test]
    fn unsafe_ids_get_bounded_safe_fingerprints() {
        let first = user_dir_name_or_fingerprint("../other");
        let repeated = user_dir_name_or_fingerprint("../other");
        let different = user_dir_name_or_fingerprint("../elsewhere");

        assert_eq!(first, repeated);
        assert_ne!(first, different);
        assert!(first.starts_with("invalid-"));
        assert!(!first.contains('/'));
        assert!(!first.contains(".."));
        assert!(first.len() < 64);
        assert_eq!(user_dir_name_or_fingerprint("user_safe-id"), "safe-id");
    }
}
