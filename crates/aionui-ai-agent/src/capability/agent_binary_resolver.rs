use std::path::PathBuf;

use aionui_api_types::{AgentMetadata, AgentSource};
use aionui_common::EnvVar;

pub(crate) fn resolve_primary_binary_candidate(meta: &AgentMetadata, binary: &str) -> Option<PathBuf> {
    if meta.agent_source == AgentSource::Builtin && meta.backend.as_deref() == Some("codex") && binary == "codex" {
        return resolve_codex_desktop_cli();
    }

    None
}

pub(crate) fn append_agent_runtime_path_env(meta: &AgentMetadata, env: &mut Vec<EnvVar>) {
    if meta.agent_source == AgentSource::Builtin
        && meta.backend.as_deref() == Some("codex")
        && let Some(codex_cli) = resolve_codex_desktop_cli()
        && let Some(codex_dir) = codex_cli.parent()
        && let Some(path) = prepend_path_entry(codex_dir.to_path_buf(), env)
    {
        if let Some(existing) = env.iter_mut().find(|entry| entry.name.eq_ignore_ascii_case("PATH")) {
            existing.value = path;
        } else {
            env.push(EnvVar {
                name: "PATH".to_owned(),
                value: path,
            });
        }
    }
}

#[cfg(windows)]
fn resolve_codex_desktop_cli() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")?;
    resolve_codex_desktop_cli_at(std::path::Path::new(&local_app_data))
}

#[cfg(not(windows))]
fn resolve_codex_desktop_cli() -> Option<PathBuf> {
    None
}

#[cfg(any(test, windows))]
fn resolve_codex_desktop_cli_at(local_app_data: &std::path::Path) -> Option<PathBuf> {
    latest_existing_codex_cli(local_app_data.join("OpenAI").join("Codex").join("bin"))
}

#[cfg(any(test, windows))]
fn latest_existing_codex_cli(bin_root: PathBuf) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(bin_root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path().join("codex.exe");
            let modified = path.metadata().and_then(|m| m.modified()).ok()?;
            path.is_file().then_some((modified, path))
        })
        .collect::<Vec<_>>();

    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, path)| path)
}

fn prepend_path_entry(entry: PathBuf, env: &[EnvVar]) -> Option<String> {
    let mut paths = vec![entry];
    let existing_path = env
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case("PATH"))
        .map(|entry| entry.value.as_str())
        .map(std::env::split_paths)
        .map(Iterator::collect::<Vec<_>>)
        .or_else(|| std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect::<Vec<_>>()));

    if let Some(existing_path) = existing_path {
        paths.extend(existing_path);
    }

    std::env::join_paths(paths)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepend_path_entry_puts_codex_dir_before_existing_path() {
        let existing_dir = std::env::temp_dir().join("aionui-existing-bin");
        let codex_dir = std::env::temp_dir().join("aionui-codex-bin");
        let existing = std::env::join_paths([existing_dir.clone()]).unwrap();
        let env = vec![EnvVar {
            name: "Path".to_owned(),
            value: existing.to_string_lossy().into_owned(),
        }];

        let merged = prepend_path_entry(codex_dir.clone(), &env).expect("merged path");
        let entries = std::env::split_paths(&merged).collect::<Vec<_>>();

        assert_eq!(entries.first(), Some(&codex_dir));
        assert!(entries.contains(&existing_dir));
    }

    #[test]
    fn latest_existing_codex_cli_uses_newest_desktop_bin() {
        use std::time::{Duration, SystemTime};

        let root = std::env::temp_dir().join(format!(
            "aionui-codex-cli-test-{}",
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let old_codex = old_dir.join("codex.exe");
        let new_codex = new_dir.join("codex.exe");
        std::fs::write(&old_codex, b"old").unwrap();
        std::thread::sleep(Duration::from_millis(10));
        std::fs::write(&new_codex, b"new").unwrap();

        let resolved = latest_existing_codex_cli(root.clone()).expect("codex fallback");
        assert_eq!(resolved, new_codex);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_codex_desktop_cli_at_uses_desktop_bin_layout() {
        let root = std::env::temp_dir().join(format!(
            "aionui-codex-desktop-layout-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let codex_dir = root.join("OpenAI").join("Codex").join("bin").join("version");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let codex = codex_dir.join("codex.exe");
        std::fs::write(&codex, b"codex").unwrap();

        let resolved = resolve_codex_desktop_cli_at(&root).expect("codex desktop fallback");
        assert_eq!(resolved, codex);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_primary_binary_candidate_only_matches_builtin_codex() {
        let mut meta = AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Codex CLI".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("codex".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: AgentSource::Builtin,
            agent_source_info: Default::default(),
            enabled: true,
            available: true,
            command: Some("codex-acp".into()),
            resolved_command: None,
            args: Vec::new(),
            env: Vec::new(),
            native_skills_dirs: None,
            behavior_policy: Default::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: false,
            handshake: Default::default(),
        };

        assert!(resolve_primary_binary_candidate(&meta, "claude").is_none());
        meta.backend = Some("claude".into());
        assert!(resolve_primary_binary_candidate(&meta, "codex").is_none());
        meta.backend = Some("codex".into());
        meta.agent_source = AgentSource::Custom;
        assert!(resolve_primary_binary_candidate(&meta, "codex").is_none());
    }
}
