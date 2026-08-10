use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const MANAGED_RESOURCES_CONTRACT_FILE: &str = "manifest.json";
pub const MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION: u8 = 2;
const SUPPORTED_RUNTIME_KEYS: [&str; 6] = [
    "win32-x64",
    "win32-arm64",
    "darwin-x64",
    "darwin-arm64",
    "linux-x64",
    "linux-arm64",
];

/// The runtime key (`<os>-<arch>`) identifying the current platform's managed
/// resources subtree. Lives beside `SUPPORTED_RUNTIME_KEYS` — the values must
/// stay in sync, and the bundled-CLI module that used to own this is gone.
pub fn current_runtime_key() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("darwin-arm64"),
        ("macos", "x86_64") => Some("darwin-x64"),
        ("linux", "aarch64") => Some("linux-arm64"),
        ("linux", "x86_64") => Some("linux-x64"),
        ("windows", "x86_64") => Some("win32-x64"),
        ("windows", "aarch64") => Some("win32-arm64"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedResourcesContract {
    pub schema_version: u8,
    pub runtime_key: String,
    pub node: ManagedNodeResourceContract,
    pub clis: Vec<ManagedCliResourceContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedNodeResourceContract {
    pub version: String,
    pub root: String,
    pub executable: String,
}

/// A bundled agent CLI (claude / codex). Unlike the removed ACP-tool contract
/// there is no node bridge or local manifest — the CLI is a native binary (plus,
/// for codex, sidecars under its `vendor/<triple>` subtree captured via
/// `required_files` / `required_directories`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedCliResourceContract {
    pub name: String,
    pub version: String,
    /// Relative to the managed-resources root, e.g. `cli/claude/2.1.215/darwin-arm64`.
    pub root: String,
    /// Must equal the contract `runtime_key`.
    pub platform_directory: String,
    /// The main executable, relative to `root` (e.g. `claude` or
    /// `vendor/aarch64-apple-darwin/bin/codex`).
    pub executable: String,
    /// Extra files that must exist relative to `root` (e.g. codex sidecars
    /// `codex-path/rg`, `codex-resources/zsh/bin/zsh`). May be empty (claude).
    #[serde(default)]
    pub required_files: Vec<String>,
    /// Extra directories that must exist relative to `root`. May be empty.
    #[serde(default)]
    pub required_directories: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ManagedResourcesContractError {
    message: String,
}

impl ManagedResourcesContractError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(action: &str, path: &Path, error: std::io::Error) -> Self {
        Self::invalid(format!("{action} {}: {error}", path.display()))
    }
}

pub fn validate_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<(), ManagedResourcesContractError> {
    validate_schema(contract)?;
    validate_node_schema(&contract.node)?;
    validate_clis_schema(contract)?;
    validate_node_paths(root, &contract.node)?;
    for cli in &contract.clis {
        validate_cli_paths(root, cli)?;
    }
    Ok(())
}

pub fn write_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<PathBuf, ManagedResourcesContractError> {
    validate_contract(root, contract)?;
    let path = root.join(MANAGED_RESOURCES_CONTRACT_FILE);
    let mut contents = serde_json::to_string_pretty(contract).map_err(|error| {
        ManagedResourcesContractError::invalid(format!("serialize managed resources contract: {error}"))
    })?;
    contents.push('\n');
    fs::write(&path, contents).map_err(|error| ManagedResourcesContractError::io("write contract", &path, error))?;
    Ok(path)
}

pub fn relative_contract_path(base: &Path, path: &Path) -> Result<String, ManagedResourcesContractError> {
    let relative = path.strip_prefix(base).map_err(|_| {
        ManagedResourcesContractError::invalid(format!(
            "path {} is not under managed resources root {}",
            path.display(),
            base.display()
        ))
    })?;
    let value = relative.to_string_lossy().replace('\\', "/");
    validate_contract_relative_path(&value)?;
    Ok(value)
}

fn validate_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    if contract.schema_version != MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION {
        return Err(ManagedResourcesContractError::invalid(format!(
            "unsupported schemaVersion {}",
            contract.schema_version
        )));
    }
    require_non_empty("runtimeKey", &contract.runtime_key)?;
    if !SUPPORTED_RUNTIME_KEYS.contains(&contract.runtime_key.as_str()) {
        return Err(ManagedResourcesContractError::invalid(format!(
            "unsupported runtimeKey {}",
            contract.runtime_key
        )));
    }
    Ok(())
}

fn validate_node_schema(node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    require_non_empty("node.version", &node.version)?;
    validate_contract_relative_path_field("node.root", &node.root)?;
    validate_contract_relative_path_field("node.executable", &node.executable)?;
    Ok(())
}

fn validate_clis_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    let mut names = HashSet::new();

    for cli in &contract.clis {
        require_non_empty("clis[].name", &cli.name)?;
        if !names.insert(cli.name.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate clis name {}",
                cli.name
            )));
        }

        let label = format!("clis[{}]", cli.name);
        require_non_empty(format!("{label}.version"), &cli.version)?;
        validate_contract_relative_path_field(format!("{label}.root"), &cli.root)?;
        require_non_empty(format!("{label}.platformDirectory"), &cli.platform_directory)?;
        if cli.platform_directory != contract.runtime_key {
            return Err(ManagedResourcesContractError::invalid(format!(
                "clis[{}].platformDirectory {} does not match runtimeKey {}",
                cli.name, cli.platform_directory, contract.runtime_key
            )));
        }
        validate_contract_relative_path_field(format!("{label}.executable"), &cli.executable)?;
        for (index, entry) in cli.required_files.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredFiles[{index}]"), entry)?;
        }
        for (index, entry) in cli.required_directories.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredDirectories[{index}]"), entry)?;
        }
    }

    // No CLI is REQUIRED to be present. claude and codex used to be demanded
    // here because the app shipped a pinned copy of each; it no longer ships
    // any, so an empty `clis` list is the normal shape and demanding entries
    // would fail every build. Entries that ARE present (an older bundle) are
    // still validated above — dropping the requirement must not mean dropping
    // the validation.
    Ok(())
}

fn validate_node_paths(root: &Path, node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    let node_root = root.join(&node.root);
    if !node_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            node_root.display()
        )));
    }
    let executable = node_root.join(&node.executable);
    if !executable.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            executable.display()
        )));
    }
    Ok(())
}

fn validate_cli_paths(root: &Path, cli: &ManagedCliResourceContract) -> Result<(), ManagedResourcesContractError> {
    let cli_root = root.join(&cli.root);
    if !cli_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            cli_root.display()
        )));
    }

    let executable = cli_root.join(&cli.executable);
    if !executable.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            executable.display()
        )));
    }

    for required_file in &cli.required_files {
        let path = cli_root.join(required_file);
        if !path.is_file() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required file missing: {}",
                path.display()
            )));
        }
    }
    for required_directory in &cli.required_directories {
        let path = cli_root.join(required_directory);
        if !path.is_dir() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required directory missing: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn require_non_empty(field: impl std::fmt::Display, value: &str) -> Result<(), ManagedResourcesContractError> {
    if value.is_empty() {
        return Err(ManagedResourcesContractError::invalid(format!("{field} is required")));
    }
    Ok(())
}

fn validate_contract_relative_path_field(
    field: impl std::fmt::Display,
    value: &str,
) -> Result<(), ManagedResourcesContractError> {
    validate_contract_relative_path(value)
        .map_err(|error| ManagedResourcesContractError::invalid(format!("{field}: {error}")))
}

fn validate_contract_relative_path(value: &str) -> Result<(), ManagedResourcesContractError> {
    if value.is_empty()
        || value.contains('\\')
        || Path::new(value).is_absolute()
        || Path::new(value)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ManagedResourcesContractError::invalid(format!(
            "invalid relative contract path {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_contract(runtime_key: &str) -> ManagedResourcesContract {
        ManagedResourcesContract {
            schema_version: MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION,
            runtime_key: runtime_key.into(),
            node: ManagedNodeResourceContract {
                version: "24.11.0".into(),
                root: "node/node-v24.11.0-win-x64".into(),
                executable: "node.exe".into(),
            },
            clis: vec![
                ManagedCliResourceContract {
                    name: "claude".into(),
                    version: "2.1.215".into(),
                    root: "cli/claude/2.1.215/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    executable: "claude.exe".into(),
                    required_files: vec![],
                    required_directories: vec![],
                },
                ManagedCliResourceContract {
                    name: "codex".into(),
                    version: "0.144.6".into(),
                    root: "cli/codex/0.144.6/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    executable: "vendor/x86_64-pc-windows-msvc/bin/codex.exe".into(),
                    required_files: vec!["vendor/x86_64-pc-windows-msvc/codex-path/rg.exe".into()],
                    required_directories: vec!["vendor/x86_64-pc-windows-msvc".into()],
                },
            ],
        }
    }

    #[test]
    fn contract_serializes_v2_camel_case_schema() {
        let contract = example_contract("win32-x64");
        let value = serde_json::to_value(&contract).expect("serialize");

        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["runtimeKey"], "win32-x64");
        assert!(value.get("schema_version").is_none());
        assert_eq!(value["clis"][0]["name"], "claude");
        assert_eq!(
            value["clis"][1]["executable"],
            "vendor/x86_64-pc-windows-msvc/bin/codex.exe"
        );
    }

    #[test]
    fn validate_contract_rejects_duplicate_cli_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis[1].name = "claude".into();

        let error = validate_contract(temp.path(), &contract).expect_err("duplicate name should fail");

        assert!(error.to_string().contains("duplicate clis name claude"));
    }

    /// The app bundles no agent CLI, so a contract that declares none is the
    /// normal shape — not a defect. This is the assertion that would have caught
    /// the packaging failure: the bundling was removed while this validator
    /// still demanded claude and codex, so every macOS build died on
    /// `missing required clis name claude`.
    #[test]
    fn validate_contract_accepts_an_empty_cli_list() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis.clear();
        // The node runtime is still bundled and still validated, so the tree has
        // to exist for this to reach the CLI question at all.
        let node_root = temp.path().join("node").join("node-v24.11.0-win-x64");
        std::fs::create_dir_all(&node_root).expect("create node root");
        std::fs::write(node_root.join("node.exe"), b"").expect("create node executable");

        validate_contract(temp.path(), &contract).expect("a bundle with no agent CLI is valid");
    }

    /// Dropping the requirement must not drop the validation: an entry that IS
    /// present (an older bundle, or a future one) is still checked.
    #[test]
    fn validate_contract_still_rejects_a_malformed_cli_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis[0].name = String::new();

        let error = validate_contract(temp.path(), &contract).expect_err("a nameless CLI entry must fail");

        assert!(error.to_string().contains("clis[].name"), "got: {error}");
    }

    #[test]
    fn validate_contract_rejects_unsafe_relative_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        for bad in ["/abs/path", "cli\\claude", "", "../escape", "cli/../escape"] {
            let mut contract = example_contract("win32-x64");
            contract.clis[0].root = bad.into();

            let error = validate_contract(temp.path(), &contract).expect_err("unsafe path should fail");

            assert!(error.to_string().contains("invalid relative contract path"), "{error}");
        }
    }

    #[test]
    fn validate_contract_rejects_platform_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.clis[0].platform_directory = "linux-x64".into();

        let error = validate_contract(temp.path(), &contract).expect_err("platform mismatch should fail");
        assert!(
            error
                .to_string()
                .contains("platformDirectory linux-x64 does not match runtimeKey win32-x64")
        );
    }

    #[test]
    fn validate_contract_rejects_missing_required_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = example_contract("win32-x64");
        std::fs::create_dir_all(temp.path().join("node").join("node-v24.11.0-win-x64")).expect("create node root");

        let error = validate_contract(temp.path(), &contract).expect_err("missing paths should fail");

        assert!(
            error.to_string().contains("required file missing")
                || error.to_string().contains("required directory missing")
        );
    }
}
