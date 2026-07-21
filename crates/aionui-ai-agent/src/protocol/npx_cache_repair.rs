use std::path::{Path, PathBuf};

use aionui_common::ErrorChain;
use tracing::warn;

#[derive(Default)]
pub(crate) struct CorruptNpxCacheRepair {
    repaired: bool,
}

impl CorruptNpxCacheRepair {
    pub(crate) fn try_repair(&mut self, stderr: &str) -> Option<PathBuf> {
        if self.repaired {
            return None;
        }

        let cache_entry = repair_corrupt_npx_cache_from_stderr(stderr)?;
        self.repaired = true;
        Some(cache_entry)
    }
}

pub(crate) fn repair_corrupt_npx_cache_from_stderr(stderr: &str) -> Option<PathBuf> {
    let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr)?;
    match std::fs::remove_dir_all(&cache_entry) {
        Ok(()) => Some(cache_entry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(cache_entry),
        Err(error) => {
            warn!(
                npm_npx_cache_entry = %cache_entry.display(),
                error = %ErrorChain(&error),
                "Failed to clear corrupt npm npx cache after ACP startup crash"
            );
            None
        }
    }
}

fn corrupt_npx_cache_entry_from_stderr(stderr: &str) -> Option<PathBuf> {
    let lower = stderr.to_ascii_lowercase();
    if !lower.contains("_npx") || !lower.contains("package.json") {
        return None;
    }
    if !lower.contains("enoent") && !lower.contains("could not read package.json") {
        return None;
    }

    stderr.lines().find_map(|line| {
        let path = parse_npm_error_path(line)?;
        npx_cache_entry_from_package_json_path(&path)
    })
}

fn parse_npm_error_path(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    for marker in ["npm error path ", "npm ERR! path "] {
        if let Some(path) = trimmed.strip_prefix(marker) {
            let path = path.trim().trim_matches('"').trim_matches('\'');
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn npx_cache_entry_from_package_json_path(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != "package.json" {
        return None;
    }
    let cache_entry = path.parent()?;
    let npx_dir = cache_entry.parent()?;
    if npx_dir.file_name()?.to_string_lossy().eq_ignore_ascii_case("_npx") {
        Some(cache_entry.to_path_buf())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{corrupt_npx_cache_entry_from_stderr, repair_corrupt_npx_cache_from_stderr};

    #[test]
    fn detects_corrupt_npm_npx_cache_entry_from_startup_stderr() {
        let stderr = "\
npm error code ENOENT
npm error syscall open
npm error path /tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3/package.json
npm error errno -2
npm error enoent Could not read package.json
";

        let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr).expect("cache entry");

        assert_eq!(
            cache_entry,
            std::path::PathBuf::from("/tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3")
        );
    }

    #[test]
    fn ignores_non_npx_package_json_startup_stderr() {
        let stderr = "\
npm error code ENOENT
npm error path /tmp/project/package.json
npm error enoent Could not read package.json
";

        assert!(corrupt_npx_cache_entry_from_stderr(stderr).is_none());
    }

    #[test]
    fn repairs_corrupt_npm_npx_cache_entry_by_removing_entry_dir() {
        let temp = tempfile::tempdir().unwrap();
        let cache_entry = temp.path().join("cache").join("_npx").join("c16927192d2e8dc3");
        std::fs::create_dir_all(&cache_entry).unwrap();
        std::fs::write(cache_entry.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(cache_entry.join("node_modules").join(".bin")).unwrap();

        let stderr = format!(
            "\
npm error code ENOENT
npm error syscall open
npm error path {}/package.json
npm error errno -2
npm error enoent Could not read package.json
",
            cache_entry.display()
        );

        let repaired = repair_corrupt_npx_cache_from_stderr(&stderr).expect("cache entry repaired");

        assert_eq!(repaired, cache_entry);
        assert!(!repaired.exists());
    }
}
