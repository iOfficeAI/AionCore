use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use aionui_api_types::RepositorySubmodule;
use walkdir::{DirEntry, WalkDir};

use crate::ProjectError;

#[derive(Debug, Default)]
pub(crate) struct DetectedProjectFacts {
    pub languages: Vec<String>,
    pub package_managers: Vec<String>,
    pub rules_files: Vec<String>,
    pub monorepo_packages: Vec<String>,
    pub submodules: Vec<RepositorySubmodule>,
    pub lfs_detected: bool,
}

pub(crate) fn detect_project(root: &Path) -> Result<DetectedProjectFacts, ProjectError> {
    let mut languages = BTreeSet::new();
    let mut package_managers = BTreeSet::new();
    let mut rules_files = BTreeSet::new();
    let mut monorepo_packages = BTreeSet::new();
    let mut lfs_detected = false;

    for entry in WalkDir::new(root)
        .follow_links(false)
        .max_depth(8)
        .into_iter()
        .filter_entry(should_visit)
    {
        let entry = entry.map_err(|error| ProjectError::Internal(format!("repository scan failed: {error}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| ProjectError::Internal(error.to_string()))?;
        let relative_text = relative.to_string_lossy().replace('\\', "/");
        let file_name = entry.file_name().to_string_lossy();
        detect_language(entry.path(), &mut languages);
        detect_package_manager(&file_name, &mut package_managers);
        if is_rules_file(&file_name) {
            rules_files.insert(relative_text.clone());
        }
        if is_package_manifest(&file_name)
            && let Some(parent) = relative.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            monorepo_packages.insert(parent.to_string_lossy().replace('\\', "/"));
        }
        if !lfs_detected && is_lfs_pointer(entry.path()) {
            lfs_detected = true;
        }
    }

    let submodules = detect_submodules(root)?;
    Ok(DetectedProjectFacts {
        languages: languages.into_iter().collect(),
        package_managers: package_managers.into_iter().collect(),
        rules_files: rules_files.into_iter().collect(),
        monorepo_packages: monorepo_packages.into_iter().collect(),
        submodules,
        lfs_detected,
    })
}

fn should_visit(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    !matches!(
        entry.file_name().to_str(),
        Some(".git" | "node_modules" | "target" | ".aion")
    )
}

fn detect_language(path: &Path, output: &mut BTreeSet<String>) {
    let language = match path.extension().and_then(|value| value.to_str()) {
        Some("rs") => Some("rust"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx" | "mjs" | "cjs") => Some("javascript"),
        Some("py") => Some("python"),
        Some("go") => Some("go"),
        Some("java" | "kt" | "kts") => Some("jvm"),
        Some("rb") => Some("ruby"),
        Some("php") => Some("php"),
        Some("cs") => Some("csharp"),
        Some("swift") => Some("swift"),
        Some("c" | "h" | "cc" | "cpp" | "hpp") => Some("cpp"),
        _ => None,
    };
    if let Some(language) = language {
        output.insert(language.into());
    }
}

fn detect_package_manager(file_name: &str, output: &mut BTreeSet<String>) {
    let manager = match file_name {
        "Cargo.toml" => Some("cargo"),
        "pnpm-lock.yaml" | "pnpm-workspace.yaml" => Some("pnpm"),
        "yarn.lock" => Some("yarn"),
        "bun.lock" | "bun.lockb" => Some("bun"),
        "package-lock.json" => Some("npm"),
        "go.mod" | "go.work" => Some("go"),
        "uv.lock" | "pyproject.toml" => Some("python"),
        "Gemfile" => Some("bundler"),
        "pom.xml" => Some("maven"),
        "build.gradle" | "build.gradle.kts" => Some("gradle"),
        _ => None,
    };
    if let Some(manager) = manager {
        output.insert(manager.into());
    }
}

fn is_rules_file(file_name: &str) -> bool {
    matches!(
        file_name,
        "AGENTS.md" | "CLAUDE.md" | "CONTRIBUTING.md" | ".cursorrules" | ".editorconfig"
    )
}

fn is_package_manifest(file_name: &str) -> bool {
    matches!(
        file_name,
        "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod" | "pom.xml"
    )
}

fn is_lfs_pointer(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() > 4096 {
        return false;
    }
    fs::read_to_string(path)
        .ok()
        .is_some_and(|value| value.starts_with("version https://git-lfs.github.com/spec/v1\n"))
}

fn detect_submodules(root: &Path) -> Result<Vec<RepositorySubmodule>, ProjectError> {
    let config_path = root.join(".gitmodules");
    if !config_path.is_file() {
        return Ok(Vec::new());
    }
    let source = fs::read_to_string(&config_path)
        .map_err(|error| ProjectError::Internal(format!("failed to read .gitmodules: {error}")))?;
    let mut output = Vec::new();
    let mut path = None;
    let mut url = None;
    let flush = |path: &mut Option<String>, url: &mut Option<String>, output: &mut Vec<RepositorySubmodule>| {
        if let Some(value) = path.take() {
            let initialized = root.join(&value).join(".git").exists();
            output.push(RepositorySubmodule {
                path: value,
                url: url.take(),
                initialized,
            });
        }
    };
    for line in source.lines().map(str::trim) {
        if line.starts_with('[') {
            flush(&mut path, &mut url, &mut output);
        } else if let Some(value) = line
            .strip_prefix("path")
            .and_then(|line| line.split_once('='))
            .map(|(_, v)| v.trim())
        {
            super::repository_source::validate_relative_path(value, "submodule path")?;
            path = Some(value.replace('\\', "/"));
        } else if let Some(value) = line
            .strip_prefix("url")
            .and_then(|line| line.split_once('='))
            .map(|(_, v)| v.trim())
        {
            url = Some(value.to_owned());
        }
    }
    flush(&mut path, &mut url, &mut output);
    output.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(output)
}

pub(crate) fn is_safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.trim().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}
