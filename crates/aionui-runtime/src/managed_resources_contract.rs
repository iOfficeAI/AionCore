use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const MANAGED_RESOURCES_CONTRACT_FILE: &str = "manifest.json";
pub const MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION: u8 = 1;
const REQUIRED_ACP_TOOL_SLUGS: [&str; 2] = ["codex-acp", "claude-agent-acp"];
const SUPPORTED_RUNTIME_KEYS: [&str; 6] = [
    "win32-x64",
    "win32-arm64",
    "darwin-x64",
    "darwin-arm64",
    "linux-x64",
    "linux-arm64",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedResourcesContract {
    pub schema_version: u8,
    pub runtime_key: String,
    pub node: ManagedNodeResourceContract,
    pub acp_tools: Vec<ManagedAcpToolResourceContract>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedNodeResourceContract {
    pub version: String,
    pub root: String,
    pub executable: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedAcpToolResourceContract {
    pub slug: String,
    pub version: String,
    pub package_name: String,
    pub root: String,
    pub platform_directory: String,
    pub manifest: String,
    pub entrypoint: String,
    pub path_entries: Vec<String>,
    pub required_files: Vec<String>,
    pub required_directories: Vec<String>,
    pub platform_executable: String,
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

#[derive(Debug, Deserialize)]
struct ToolLocalManifest {
    entrypoint: String,
    #[serde(default)]
    path_entries: Vec<String>,
}

pub fn validate_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<(), ManagedResourcesContractError> {
    validate_schema(contract)?;
    validate_node_schema(&contract.node)?;
    validate_acp_tools_schema(contract)?;
    validate_node_paths(root, &contract.node)?;
    for tool in &contract.acp_tools {
        validate_acp_tool_paths(root, tool)?;
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

fn validate_acp_tools_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    let mut slugs = HashSet::new();

    for tool in &contract.acp_tools {
        require_non_empty("acpTools[].slug", &tool.slug)?;
        if !slugs.insert(tool.slug.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate acpTools slug {}",
                tool.slug
            )));
        }

        let label = format!("acpTools[{}]", tool.slug);
        require_non_empty(format!("{label}.version"), &tool.version)?;
        require_non_empty(format!("{label}.packageName"), &tool.package_name)?;
        validate_contract_relative_path_field(format!("{label}.root"), &tool.root)?;
        require_non_empty(format!("{label}.platformDirectory"), &tool.platform_directory)?;
        if tool.platform_directory != contract.runtime_key {
            return Err(ManagedResourcesContractError::invalid(format!(
                "acpTools[{}].platformDirectory {} does not match runtimeKey {}",
                tool.slug, tool.platform_directory, contract.runtime_key
            )));
        }
        validate_contract_relative_path_field(format!("{label}.manifest"), &tool.manifest)?;
        validate_contract_relative_path_field(format!("{label}.entrypoint"), &tool.entrypoint)?;
        for (index, entry) in tool.path_entries.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.pathEntries[{index}]"), entry)?;
        }
        if tool.required_files.is_empty() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "{label}.requiredFiles must not be empty"
            )));
        }
        for (index, entry) in tool.required_files.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredFiles[{index}]"), entry)?;
        }
        if tool.required_directories.is_empty() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "{label}.requiredDirectories must not be empty"
            )));
        }
        for (index, entry) in tool.required_directories.iter().enumerate() {
            validate_contract_relative_path_field(format!("{label}.requiredDirectories[{index}]"), entry)?;
        }
        validate_contract_relative_path_field(format!("{label}.platformExecutable"), &tool.platform_executable)?;
    }

    for required_slug in REQUIRED_ACP_TOOL_SLUGS {
        if !slugs.contains(required_slug) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "missing required acpTools slug {required_slug}"
            )));
        }
    }

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

fn validate_acp_tool_paths(
    root: &Path,
    tool: &ManagedAcpToolResourceContract,
) -> Result<(), ManagedResourcesContractError> {
    let tool_root = root.join(&tool.root);
    if !tool_root.is_dir() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required directory missing: {}",
            tool_root.display()
        )));
    }

    let manifest_path = tool_root.join(&tool.manifest);
    if !manifest_path.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            manifest_path.display()
        )));
    }
    let local_manifest = read_tool_local_manifest(&manifest_path)?;
    if local_manifest.entrypoint != tool.entrypoint {
        return Err(ManagedResourcesContractError::invalid(format!(
            "local manifest entrypoint mismatch for {}: expected {}, got {}",
            tool.slug, tool.entrypoint, local_manifest.entrypoint
        )));
    }
    if local_manifest.path_entries != tool.path_entries {
        return Err(ManagedResourcesContractError::invalid(format!(
            "local manifest path_entries mismatch for {}",
            tool.slug
        )));
    }

    let entrypoint = tool_root.join(&tool.entrypoint);
    if !entrypoint.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            entrypoint.display()
        )));
    }

    for required_file in &tool.required_files {
        let path = tool_root.join(required_file);
        if !path.is_file() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required file missing: {}",
                path.display()
            )));
        }
    }
    for required_directory in &tool.required_directories {
        let path = tool_root.join(required_directory);
        if !path.is_dir() {
            return Err(ManagedResourcesContractError::invalid(format!(
                "required directory missing: {}",
                path.display()
            )));
        }
    }

    let executable = tool_root.join(&tool.platform_executable);
    if !executable.is_file() {
        return Err(ManagedResourcesContractError::invalid(format!(
            "required file missing: {}",
            executable.display()
        )));
    }

    Ok(())
}

fn read_tool_local_manifest(path: &Path) -> Result<ToolLocalManifest, ManagedResourcesContractError> {
    let contents = fs::read_to_string(path)
        .map_err(|error| ManagedResourcesContractError::io("read local manifest", path, error))?;
    serde_json::from_str(&contents).map_err(|error| {
        ManagedResourcesContractError::invalid(format!(
            "parse local managed ACP manifest failed for {}: {error}",
            path.display()
        ))
    })
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
            acp_tools: vec![
                ManagedAcpToolResourceContract {
                    slug: "codex-acp".into(),
                    version: "1.1.2".into(),
                    package_name: "@agentclientprotocol/codex-acp".into(),
                    root: "acp/codex-acp/1.1.2/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    manifest: "manifest.json".into(),
                    entrypoint: "node_modules/@agentclientprotocol/codex-acp/dist/index.js".into(),
                    path_entries: vec!["node_modules/.bin".into()],
                    required_files: vec!["package.json".into(), "package-lock.json".into()],
                    required_directories: vec!["node_modules".into()],
                    platform_executable:
                        "node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe".into(),
                },
                ManagedAcpToolResourceContract {
                    slug: "claude-agent-acp".into(),
                    version: "0.58.1".into(),
                    package_name: "@agentclientprotocol/claude-agent-acp".into(),
                    root: "acp/claude-agent-acp/0.58.1/win32-x64".into(),
                    platform_directory: "win32-x64".into(),
                    manifest: "manifest.json".into(),
                    entrypoint: "node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js".into(),
                    path_entries: vec!["node_modules/.bin".into()],
                    required_files: vec!["package.json".into(), "package-lock.json".into()],
                    required_directories: vec!["node_modules".into()],
                    platform_executable: "node_modules/@anthropic-ai/claude-agent-sdk-win32-x64/claude.exe".into(),
                },
            ],
        }
    }

    #[test]
    fn contract_serializes_v1_camel_case_schema() {
        let contract = example_contract("win32-x64");
        let value = serde_json::to_value(&contract).expect("serialize");

        assert_eq!(value["schemaVersion"], 1);
        assert_eq!(value["runtimeKey"], "win32-x64");
        assert!(value.get("schema_version").is_none());
        assert_eq!(value["acpTools"][0]["packageName"], "@agentclientprotocol/codex-acp");
        assert_eq!(
            value["acpTools"][0]["requiredFiles"],
            serde_json::json!(["package.json", "package-lock.json"])
        );
        assert_eq!(
            value["acpTools"][0]["requiredDirectories"],
            serde_json::json!(["node_modules"])
        );
    }

    #[test]
    fn validate_contract_rejects_duplicate_tool_slugs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.acp_tools[1].slug = "codex-acp".into();

        let error = validate_contract(temp.path(), &contract).expect_err("duplicate slug should fail");

        assert!(error.to_string().contains("duplicate acpTools slug codex-acp"));
    }

    #[test]
    fn validate_contract_rejects_missing_required_tool_slug() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.acp_tools.retain(|tool| tool.slug != "claude-agent-acp");

        let error = validate_contract(temp.path(), &contract).expect_err("missing required slug should fail");

        assert!(
            error
                .to_string()
                .contains("missing required acpTools slug claude-agent-acp")
        );
    }

    #[test]
    fn validate_contract_rejects_unsafe_relative_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        for bad in ["/abs/path", "acp\\codex-acp", "", "../escape", "acp/../escape"] {
            let mut contract = example_contract("win32-x64");
            contract.acp_tools[0].root = bad.into();

            let error = validate_contract(temp.path(), &contract).expect_err("unsafe path should fail");

            assert!(error.to_string().contains("invalid relative contract path"), "{error}");
        }
    }

    #[test]
    fn validate_contract_rejects_empty_required_strings_and_platform_mismatch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = example_contract("win32-x64");
        contract.acp_tools[0].package_name.clear();

        let error = validate_contract(temp.path(), &contract).expect_err("empty package name should fail");
        assert!(
            error
                .to_string()
                .contains("acpTools[codex-acp].packageName is required")
        );

        let mut contract = example_contract("win32-x64");
        contract.acp_tools[0].platform_directory = "linux-x64".into();

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

        assert!(error.to_string().contains("required file missing"));
    }
}
