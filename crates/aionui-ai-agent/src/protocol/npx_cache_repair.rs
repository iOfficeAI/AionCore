use std::path::{Path, PathBuf};

use aionui_common::CommandSpec;
use aionui_common::ErrorChain;
use sha2::{Digest, Sha512};
use tracing::warn;

#[derive(Default)]
pub(crate) struct CorruptNpxCacheRepair {
    repaired: bool,
}

impl CorruptNpxCacheRepair {
    pub(crate) fn try_repair(&mut self, command_spec: &CommandSpec, stderr: &str) -> Option<PathBuf> {
        if self.repaired {
            return None;
        }

        let cache_entry = repair_corrupt_npx_cache(command_spec, stderr)?;
        self.repaired = true;
        Some(cache_entry)
    }
}

pub(crate) fn repair_corrupt_npx_cache(command_spec: &CommandSpec, stderr: &str) -> Option<PathBuf> {
    if let Some(cache_entry) = computed_existing_npx_cache_entry(command_spec)
        && clear_npx_cache_entry(&cache_entry, MissingEntryPolicy::Skip).is_some()
    {
        return Some(cache_entry);
    }

    let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr)?;
    clear_npx_cache_entry(&cache_entry, MissingEntryPolicy::Repaired)
}

fn computed_existing_npx_cache_entry(command_spec: &CommandSpec) -> Option<PathBuf> {
    let npx_cache_entry = computed_npx_cache_entry(command_spec)?;
    if !npx_cache_entry.exists() {
        return None;
    }
    Some(npx_cache_entry)
}

fn computed_npx_cache_entry(command_spec: &CommandSpec) -> Option<PathBuf> {
    let npm_cache = command_spec_env(command_spec, "npm_config_cache")?;
    let packages = npx_package_specs(command_spec)?;
    let hash = npm_npx_cache_hash(&packages);
    Some(PathBuf::from(npm_cache).join("_npx").join(hash))
}

fn command_spec_env<'a>(command_spec: &'a CommandSpec, name: &str) -> Option<&'a str> {
    command_spec
        .env
        .iter()
        .rev()
        .find(|entry| entry.name == name)
        .map(|entry| entry.value.as_str())
}

fn npx_package_specs(command_spec: &CommandSpec) -> Option<Vec<String>> {
    let npx_args = npx_cli_args(command_spec)?;
    let packages = explicit_npx_package_specs(npx_args);
    if !packages.is_empty() {
        return Some(packages);
    }

    first_npx_positional_package(npx_args).map(|package| vec![package])
}

fn npx_cli_args(command_spec: &CommandSpec) -> Option<&[String]> {
    let command_name = command_spec.command.file_name()?.to_string_lossy();
    if is_npx_executable(&command_name) {
        return Some(command_spec.args.as_slice());
    }

    let (first, rest) = command_spec.args.split_first()?;
    if first.ends_with("npx-cli.js") || first.ends_with("npx-cli.cjs") {
        return Some(rest);
    }

    None
}

fn is_npx_executable(command_name: &str) -> bool {
    command_name == "npx" || command_name == "npx.cmd" || command_name == "npx.exe"
}

fn explicit_npx_package_specs(args: &[String]) -> Vec<String> {
    let mut packages = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--package" || arg == "-p" {
            if let Some(value) = iter.next()
                && !value.trim().is_empty()
            {
                packages.push(value.clone());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("--package=") {
            if !value.trim().is_empty() {
                packages.push(value.to_owned());
            }
            continue;
        }
        if let Some(value) = arg.strip_prefix("-p=")
            && !value.trim().is_empty()
        {
            packages.push(value.to_owned());
        }
    }
    packages
}

fn first_npx_positional_package(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().filter(|value| !value.trim().is_empty()).cloned();
        }
        if arg == "--package"
            || arg == "-p"
            || arg == "--cache"
            || arg == "--userconfig"
            || arg == "--call"
            || arg == "-c"
            || arg == "--script-shell"
            || arg == "--shell"
        {
            let _ = iter.next();
            continue;
        }
        if arg.starts_with("--package=")
            || arg.starts_with("-p=")
            || arg.starts_with("--cache=")
            || arg.starts_with("--userconfig=")
            || arg.starts_with("--call=")
            || arg.starts_with("-c=")
            || arg.starts_with("--script-shell=")
            || arg.starts_with("--shell=")
        {
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn npm_npx_cache_hash(packages: &[String]) -> String {
    let mut packages = packages.to_vec();
    packages.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let input = packages.join("\n");
    let digest = Sha512::digest(input.as_bytes());
    hex::encode(digest)[..16].to_owned()
}

enum MissingEntryPolicy {
    Repaired,
    Skip,
}

fn clear_npx_cache_entry(cache_entry: &Path, missing_policy: MissingEntryPolicy) -> Option<PathBuf> {
    match std::fs::remove_dir_all(cache_entry) {
        Ok(()) => Some(cache_entry.to_path_buf()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match missing_policy {
            MissingEntryPolicy::Repaired => Some(cache_entry.to_path_buf()),
            MissingEntryPolicy::Skip => None,
        },
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
    if !lower.contains("_npx") {
        return None;
    }
    if !has_corrupt_npx_cache_signal(&lower) {
        return None;
    }

    find_npx_cache_entry_path(stderr)
}

fn has_corrupt_npx_cache_signal(lower: &str) -> bool {
    lower.contains("enoent")
        || lower.contains("could not read package.json")
        || lower.contains("cannot find module")
        || lower.contains("no such file or directory")
}

fn find_npx_cache_entry_path(stderr: &str) -> Option<PathBuf> {
    let bytes = stderr.as_bytes();
    let lower = stderr.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_index) = lower[offset..].find("_npx") {
        let marker_index = offset + relative_index;
        if let Some(cache_entry) = npx_cache_entry_around_marker(stderr, bytes, marker_index) {
            return Some(cache_entry);
        }
        offset = marker_index + "_npx".len();
    }

    None
}

fn npx_cache_entry_around_marker(stderr: &str, bytes: &[u8], marker_index: usize) -> Option<PathBuf> {
    let start = path_start_before_marker(bytes, marker_index);
    let end = path_end_after_marker(bytes, marker_index);
    let candidate = stderr.get(start..end)?.trim_matches(path_quote_or_punctuation);
    cache_entry_from_npx_path(candidate)
}

fn path_start_before_marker(bytes: &[u8], marker_index: usize) -> usize {
    let mut start = marker_index;
    while start > 0 {
        let byte = bytes[start - 1];
        if is_path_boundary(byte) {
            break;
        }
        start -= 1;
    }
    start
}

fn path_end_after_marker(bytes: &[u8], marker_index: usize) -> usize {
    let mut end = marker_index + "_npx".len();
    while end < bytes.len() {
        let byte = bytes[end];
        if is_path_boundary(byte) {
            break;
        }
        end += 1;
    }
    end
}

fn is_path_boundary(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'"' | b'\'' | b'`' | b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'{' | b'}'
        )
}

fn path_quote_or_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':'
    )
}

fn cache_entry_from_npx_path(path: &str) -> Option<PathBuf> {
    let parts = split_path_with_separators(path);
    let npx_index = parts
        .iter()
        .position(|part| part.segment.eq_ignore_ascii_case("_npx"))?;
    if npx_index == 0 {
        return None;
    }
    let entry_index = npx_index + 1;
    if parts.get(entry_index)?.segment.is_empty() {
        return None;
    }

    let end = parts[entry_index].end;
    Some(PathBuf::from(path[..end].to_owned()))
}

#[derive(Debug)]
struct PathPart<'a> {
    segment: &'a str,
    end: usize,
}

fn split_path_with_separators(path: &str) -> Vec<PathPart<'_>> {
    let mut parts = Vec::new();
    let mut start = 0;

    for (index, ch) in path.char_indices() {
        if ch == '/' || ch == '\\' {
            if start < index {
                parts.push(PathPart {
                    segment: &path[start..index],
                    end: index,
                });
            }
            start = index + ch.len_utf8();
        }
    }

    if start < path.len() {
        parts.push(PathPart {
            segment: &path[start..],
            end: path.len(),
        });
    }

    parts
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use aionui_common::{CommandSpec, EnvVar};

    use super::{
        computed_npx_cache_entry, corrupt_npx_cache_entry_from_stderr, npm_npx_cache_hash, repair_corrupt_npx_cache,
    };

    fn npx_command_spec(cache: &Path, args: &[&str]) -> CommandSpec {
        CommandSpec {
            command: PathBuf::from("npx"),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: vec![EnvVar {
                name: "npm_config_cache".to_owned(),
                value: cache.display().to_string(),
            }],
            cwd: None,
        }
    }

    fn node_npx_cli_command_spec(cache: &Path, args: &[&str]) -> CommandSpec {
        let mut all_args = vec![r"C:\AionUi\runtime\node\node_modules\npm\bin\npx-cli.js".to_owned()];
        all_args.extend(args.iter().map(|arg| (*arg).to_owned()));
        CommandSpec {
            command: PathBuf::from("node.exe"),
            args: all_args,
            env: vec![EnvVar {
                name: "npm_config_cache".to_owned(),
                value: cache.display().to_string(),
            }],
            cwd: None,
        }
    }

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
    fn detects_corrupt_npm_npx_cache_entry_from_quoted_enoent_path() {
        let stderr = "\
Error: ENOENT: no such file or directory, open '/tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3/node_modules/@xai/grok-cli/package.json'
";

        let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr).expect("cache entry");

        assert_eq!(
            cache_entry,
            std::path::PathBuf::from("/tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3")
        );
    }

    #[test]
    fn detects_corrupt_npm_npx_cache_entry_from_missing_bin_path() {
        let stderr = "\
sh: /tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3/node_modules/.bin/grok: No such file or directory
";

        let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr).expect("cache entry");

        assert_eq!(
            cache_entry,
            std::path::PathBuf::from("/tmp/aionui/runtime/node/cache/_npx/c16927192d2e8dc3")
        );
    }

    #[test]
    fn detects_corrupt_npm_npx_cache_entry_from_windows_path() {
        let stderr = r#"
npm ERR! enoent ENOENT: no such file or directory, open 'C:\Users\Alice\AppData\Local\npm-cache\_npx\c16927192d2e8dc3\package.json'
"#;

        let cache_entry = corrupt_npx_cache_entry_from_stderr(stderr).expect("cache entry");

        assert_eq!(
            cache_entry,
            std::path::PathBuf::from(r"C:\Users\Alice\AppData\Local\npm-cache\_npx\c16927192d2e8dc3")
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
    fn ignores_relative_npx_path() {
        let stderr = "\
npm error code ENOENT
npm error path _npx/c16927192d2e8dc3/package.json
npm error enoent Could not read package.json
";

        assert!(corrupt_npx_cache_entry_from_stderr(stderr).is_none());
    }

    #[test]
    fn repairs_corrupt_npm_npx_cache_entry_by_removing_entry_dir() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_command_spec(temp.path().join("cache").as_path(), &["-y", "other-package"]);
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

        let repaired = repair_corrupt_npx_cache(&spec, &stderr).expect("cache entry repaired");

        assert_eq!(repaired, cache_entry);
        assert!(!repaired.exists());
    }

    #[test]
    fn computes_npx_cache_entry_from_direct_package_argument() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_command_spec(temp.path(), &["-y", "@xai-official/grok@0.2.102", "agent", "stdio"]);

        let cache_entry = computed_npx_cache_entry(&spec).expect("computed cache entry");

        assert_eq!(cache_entry, temp.path().join("_npx").join("c16927192d2e8dc3"));
    }

    #[test]
    fn computes_npx_cache_entry_from_package_flag() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_command_spec(
            temp.path(),
            &[
                "-y",
                "--package",
                "@tencent-ai/codebuddy-code@2.106.7",
                "codebuddy",
                "--acp",
            ],
        );

        let cache_entry = computed_npx_cache_entry(&spec).expect("computed cache entry");

        assert_eq!(cache_entry, temp.path().join("_npx").join("fc1de2abdabf8717"));
    }

    #[test]
    fn computes_npx_cache_entry_from_node_wrapped_npx_cli() {
        let temp = tempfile::tempdir().unwrap();
        let spec = node_npx_cli_command_spec(temp.path(), &["-y", "pi-acp@0.0.31"]);

        let cache_entry = computed_npx_cache_entry(&spec).expect("computed cache entry");

        assert_eq!(cache_entry, temp.path().join("_npx").join("dc42e4d625bccf60"));
    }

    #[test]
    fn computes_npx_cache_hash_from_sorted_package_specs() {
        assert_eq!(
            npm_npx_cache_hash(&["z-package@1.0.0".to_owned(), "@scope/package@2.0.0".to_owned(),]),
            npm_npx_cache_hash(&["@scope/package@2.0.0".to_owned(), "z-package@1.0.0".to_owned(),])
        );
    }

    #[test]
    fn repairs_computed_npx_cache_entry_without_stderr_path() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_command_spec(temp.path(), &["-y", "@xai-official/grok@0.2.102", "agent", "stdio"]);
        let cache_entry = temp.path().join("_npx").join("c16927192d2e8dc3");
        std::fs::create_dir_all(cache_entry.join("node_modules")).unwrap();

        let repaired = repair_corrupt_npx_cache(&spec, "agent exited before initialize").expect("cache entry repaired");

        assert_eq!(repaired, cache_entry);
        assert!(!repaired.exists());
    }

    #[test]
    fn repairs_computed_npx_cache_entry_even_when_package_json_exists() {
        let temp = tempfile::tempdir().unwrap();
        let spec = npx_command_spec(temp.path(), &["-y", "@xai-official/grok@0.2.102", "agent", "stdio"]);
        let cache_entry = temp.path().join("_npx").join("c16927192d2e8dc3");
        std::fs::create_dir_all(&cache_entry).unwrap();
        std::fs::write(cache_entry.join("package.json"), "{}").unwrap();

        let repaired = repair_corrupt_npx_cache(&spec, "agent exited before initialize");

        assert_eq!(repaired, Some(cache_entry.clone()));
        assert!(!cache_entry.exists());
    }
}
