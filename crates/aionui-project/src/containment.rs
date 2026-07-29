use std::path::{Component, Path, PathBuf};

use crate::canonical::{self, Canonical};
use crate::types::{FileOp, ProjectError};

/// A `relative_path` resolved against a folder root: the normalized relative
/// path plus the derived child resource URI / absolute path. Pure lexical
/// result — no filesystem IO, no symlink resolution.
#[derive(Debug, Clone)]
pub struct ResolvedRelative {
    /// Forward-slash normalized relative path, no leading slash, no `.`/`..`.
    /// Empty means the folder root itself.
    pub relative_path: String,
    /// Child resource provider URI (`file:` derived).
    pub resource_uri: String,
    /// Absolute filesystem path (`file:` derived).
    pub absolute_path: Option<PathBuf>,
}

/// Resolve `relative_path` under `root`, rejecting absolute paths and `..`
/// escapes lexically. The realpath/symlink escape check is a runtime-chain
/// concern performed just before IO, not here.
pub fn resolve_relative(root: &Canonical, relative_path: &str, _op: FileOp) -> Result<ResolvedRelative, ProjectError> {
    let mut segs: Vec<String> = Vec::new();
    for comp in Path::new(relative_path).components() {
        match comp {
            // An absolute path (leading slash or Windows prefix) is not a
            // valid relative reference.
            Component::Prefix(_) | Component::RootDir => {
                return Err(ProjectError::InvalidRelativePath {
                    relative_path: relative_path.to_owned(),
                });
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // `..` that would ascend above the root is an escape.
                if segs.pop().is_none() {
                    return Err(ProjectError::InvalidRelativePath {
                        relative_path: relative_path.to_owned(),
                    });
                }
            }
            Component::Normal(seg) => segs.push(seg.to_string_lossy().into_owned()),
        }
    }

    let normalized = segs.join("/");
    let root_path = canonical::fs_path(root)?;
    let absolute = if normalized.is_empty() {
        root_path.clone()
    } else {
        root_path.join(&normalized)
    };

    // Defensive same-or-descendant guard. Normalized paths carry no `..`, so
    // this only fires on pathological input; symlink escapes are a runtime check.
    if !absolute.starts_with(&root_path) {
        return Err(ProjectError::ResourceOutsideFolder {
            relative_path: relative_path.to_owned(),
        });
    }

    let resource_uri = canonical::to_file_uri(&absolute)?;
    Ok(ResolvedRelative {
        relative_path: normalized,
        resource_uri,
        absolute_path: Some(absolute),
    })
}

#[cfg(test)]
#[path = "containment_test.rs"]
mod containment_test;
