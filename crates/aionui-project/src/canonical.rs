use std::path::{Component, Path, PathBuf};

use url::Url;

use crate::types::ProjectError;

/// Whether the `file:` provider folds path casing on this platform.
/// macOS / Windows are case-insensitive; Linux is case-sensitive. Making this
/// a compile-time constant keeps `resource_canonical` machine-platform-relative
/// as the design specifies (local single-DB desktop, so no cross-platform key).
pub const IGNORE_PATH_CASING: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Provider scheme. Only `file:` is registered for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    File,
}

/// A lexically-normalized canonical resource URI — the folder dedupe key.
/// Produced only by [`canonicalize`]; never constructed from a raw string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical(String);

impl Canonical {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parse the scheme of a provider URI (for provider dispatch; not persisted).
pub fn parse_scheme(uri: &str) -> Result<Scheme, ProjectError> {
    let url = Url::parse(uri).map_err(|_| ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() })?;
    match url.scheme() {
        "file" => Ok(Scheme::File),
        other => Err(ProjectError::UnsupportedResourceScheme {
            scheme: other.to_owned(),
        }),
    }
}

/// Lexical canonicalization of a `file:` URI into the dedupe-key identity.
/// Pure: never touches the filesystem, never resolves symlinks.
///
/// Steps (see `data-model.md` "resource_canonical semantics"): parse to a fs
/// path, lexically resolve `.`/`..` and collapse separators, drop trailing
/// slash, fold path casing per platform, rebuild the `file:` URI (percent-
/// encoded, no fragment/query).
pub fn canonicalize(uri: &str) -> Result<Canonical, ProjectError> {
    // Only file: is supported; other schemes are rejected up front.
    parse_scheme(uri)?;

    let url = Url::parse(uri).map_err(|_| ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() })?;
    let path = url
        .to_file_path()
        .map_err(|_| ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() })?;

    let normalized = normalize_lexical(&path);
    let folded = if IGNORE_PATH_CASING {
        PathBuf::from(normalized.to_string_lossy().to_ascii_lowercase())
    } else {
        normalized
    };

    // from_file_path yields a percent-encoded file: URI with no trailing slash
    // and no fragment/query.
    let canonical_url =
        Url::from_file_path(&folded).map_err(|_| ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() })?;
    Ok(Canonical(canonical_url.to_string()))
}

/// Display-name derivation: the final path segment of a canonical folder.
pub fn basename(canonical: &Canonical) -> String {
    fs_path(canonical)
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Build a `file:` URI from an absolute filesystem path (raw input capture).
/// Does not canonicalize — callers pass the result to [`canonicalize`] for the
/// identity form.
pub fn to_file_uri(path: &Path) -> Result<String, ProjectError> {
    Url::from_file_path(path)
        .map(|u| u.to_string())
        .map_err(|_| ProjectError::FolderCanonicalizeFailed {
            uri: path.to_string_lossy().into_owned(),
        })
}

/// Derive the absolute filesystem path from a `file:` canonical URI.
pub fn fs_path(canonical: &Canonical) -> Result<PathBuf, ProjectError> {
    uri_to_path(canonical.as_str())
}

/// Derive the absolute filesystem path from a raw `file:` URI string. Used by
/// the runtime provider, which receives already-resolved URIs as `&str` (a
/// [`Canonical`] cannot be constructed outside [`canonicalize`]).
pub fn uri_to_path(uri: &str) -> Result<PathBuf, ProjectError> {
    Url::parse(uri)
        .ok()
        .and_then(|u| u.to_file_path().ok())
        .ok_or_else(|| ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() })
}

/// Lexically resolve `.` / `..` and collapse redundant separators without
/// touching the filesystem. `..` above the root is clamped (not an error here —
/// containment rejects escapes at the reference layer).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
                // Otherwise ignore: cannot ascend above root / prefix.
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

#[cfg(test)]
#[path = "canonical_test.rs"]
mod canonical_test;
