use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

pub const MANAGED_RESOURCES_CONTRACT_FILE: &str = "manifest.json";
pub const MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION: u8 = 3;
const LEGACY_SCHEMA_VERSION: u8 = 2;
const LEGACY_REQUIRED_CLI_NAMES: [&str; 2] = ["claude", "codex"];
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
    pub clis: Vec<ManagedCliResourceContract>,
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
pub struct ManagedCliResourceContract {
    pub name: String,
    pub version: String,
    /// Relative to the managed-resources root.
    pub root: String,
    /// Must equal the contract `runtime_key`.
    #[serde(rename = "target", alias = "platformDirectory")]
    pub platform_directory: String,
    /// Legacy main executable relative to `root`. V3 launch uses
    /// `launch.program`; this remains for the existing prepare/export flow.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub executable: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_directories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<ManagedCliLaunchContract>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<ManagedCliFileContract>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capabilities: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedCliLaunchContract {
    /// Program path relative to the CLI root.
    pub program: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args_prefix: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<ManagedCliLaunchEnvContract>,
    /// Directories relative to the CLI root that are prepended to PATH.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_entries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedCliLaunchEnvContract {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedCliFileContract {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedResourcesContractErrorCode {
    Io,
    MalformedJson,
    UnsupportedSchema,
    Invalid,
    MissingPath,
    PathEscape,
    InvalidHash,
    HashMismatch,
}

impl ManagedResourcesContractErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::MalformedJson => "malformed_json",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::Invalid => "invalid",
            Self::MissingPath => "missing_path",
            Self::PathEscape => "path_escape",
            Self::InvalidHash => "invalid_hash",
            Self::HashMismatch => "hash_mismatch",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ManagedResourcesContractError {
    code: ManagedResourcesContractErrorCode,
    message: String,
}

impl ManagedResourcesContractError {
    fn new(code: ManagedResourcesContractErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ManagedResourcesContractErrorCode::Invalid, message)
    }

    fn io(action: &str, path: &Path, error: std::io::Error) -> Self {
        Self::new(
            ManagedResourcesContractErrorCode::Io,
            format!("{action} {}: {error}", path.display()),
        )
    }

    pub const fn code(&self) -> ManagedResourcesContractErrorCode {
        self.code
    }
}

/// Read and structurally validate a v2 or v3 managed-resources manifest.
///
/// V2 is accepted only for compatibility probing. A selected v2 CLI cannot be
/// launched as managed because it has no authenticated launch contract.
pub fn read_contract(root: &Path) -> Result<ManagedResourcesContract, ManagedResourcesContractError> {
    let path = root.join(MANAGED_RESOURCES_CONTRACT_FILE);
    let canonical_root = canonical_directory(root, "managed resources root")?;
    let canonical_path = canonical_file_under(&canonical_root, &path, "managed resources contract")?;
    let contents = fs::read(&canonical_path)
        .map_err(|error| ManagedResourcesContractError::io("read contract", &canonical_path, error))?;
    let contract: ManagedResourcesContract = serde_json::from_slice(&contents).map_err(|error| {
        ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MalformedJson,
            format!("parse managed resources contract {}: {error}", path.display()),
        )
    })?;
    validate_loaded_schema(&contract)?;
    Ok(contract)
}

/// Validate the complete v3 contract and all of its materialized resources.
pub fn validate_contract(
    root: &Path,
    contract: &ManagedResourcesContract,
) -> Result<(), ManagedResourcesContractError> {
    validate_v3_schema(contract)?;
    validate_node_paths(root, &contract.node)?;
    for cli in &contract.clis {
        validate_cli_entry(root, contract, cli)?;
    }
    Ok(())
}

/// Validate only the named v3 CLI and its launch assets.
///
/// This deliberately avoids checking materialized files belonging to unrelated
/// CLI entries so one optional tool cannot make every managed tool unavailable.
pub fn validate_cli(
    root: &Path,
    contract: &ManagedResourcesContract,
    name: &str,
) -> Result<(), ManagedResourcesContractError> {
    validate_loaded_schema(contract)?;
    if contract.schema_version != MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::UnsupportedSchema,
            format!(
                "managed launch for {name} requires schemaVersion {MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION}, got {}",
                contract.schema_version
            ),
        ));
    }
    let cli = contract
        .clis
        .iter()
        .find(|cli| cli.name == name)
        .ok_or_else(|| ManagedResourcesContractError::invalid(format!("CLI {name} is not declared")))?;
    validate_cli_entry(root, contract, cli)
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

/// Hash every file under `root` and return a stable path-sorted list.
pub fn collect_file_hashes(root: &Path) -> Result<Vec<ManagedCliFileContract>, ManagedResourcesContractError> {
    let canonical_root = canonical_directory(root, "CLI hash root")?;
    let mut files = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| {
            ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::Io,
                format!("walk managed CLI root {}: {error}", root.display()),
            )
        })?;
        if !entry.path().is_file() {
            continue;
        }
        let canonical_path = fs::canonicalize(entry.path())
            .map_err(|error| ManagedResourcesContractError::io("canonicalize file", entry.path(), error))?;
        ensure_contained(&canonical_root, &canonical_path, "hashed file")?;
        let path = relative_contract_path(root, entry.path())?;
        files.push(ManagedCliFileContract {
            path,
            sha256: hash_file(entry.path())?,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn validate_loaded_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    match contract.schema_version {
        LEGACY_SCHEMA_VERSION | MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION => {}
        version => {
            return Err(ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::UnsupportedSchema,
                format!("unsupported schemaVersion {version}"),
            ));
        }
    }
    require_non_empty("runtimeKey", &contract.runtime_key)?;
    if !SUPPORTED_RUNTIME_KEYS.contains(&contract.runtime_key.as_str()) {
        return Err(ManagedResourcesContractError::invalid(format!(
            "unsupported runtimeKey {}",
            contract.runtime_key
        )));
    }
    validate_node_schema(&contract.node)?;

    let mut names = HashSet::new();
    for cli in &contract.clis {
        validate_cli_common_schema(contract, cli)?;
        if !names.insert(cli.name.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate clis name {}",
                cli.name
            )));
        }
    }
    if contract.schema_version == LEGACY_SCHEMA_VERSION {
        for required_name in LEGACY_REQUIRED_CLI_NAMES {
            if !names.contains(required_name) {
                return Err(ManagedResourcesContractError::invalid(format!(
                    "schemaVersion 2 is missing required clis name {required_name}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_v3_schema(contract: &ManagedResourcesContract) -> Result<(), ManagedResourcesContractError> {
    validate_loaded_schema(contract)?;
    if contract.schema_version != MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::UnsupportedSchema,
            format!(
                "write/validate requires schemaVersion {MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION}, got {}",
                contract.schema_version
            ),
        ));
    }
    for cli in &contract.clis {
        validate_cli_launch_schema(cli)?;
    }
    Ok(())
}

fn validate_node_schema(node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    require_non_empty("node.version", &node.version)?;
    validate_contract_relative_path_field("node.root", &node.root)?;
    validate_contract_relative_path_field("node.executable", &node.executable)?;
    Ok(())
}

fn validate_cli_common_schema(
    contract: &ManagedResourcesContract,
    cli: &ManagedCliResourceContract,
) -> Result<(), ManagedResourcesContractError> {
    require_non_empty("clis[].name", &cli.name)?;
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
    if contract.schema_version == LEGACY_SCHEMA_VERSION || !cli.executable.is_empty() {
        validate_contract_relative_path_field(format!("{label}.executable"), &cli.executable)?;
    }
    for (index, entry) in cli.required_files.iter().enumerate() {
        validate_contract_relative_path_field(format!("{label}.requiredFiles[{index}]"), entry)?;
    }
    for (index, entry) in cli.required_directories.iter().enumerate() {
        validate_contract_relative_path_field(format!("{label}.requiredDirectories[{index}]"), entry)?;
    }
    Ok(())
}

fn validate_cli_launch_schema(cli: &ManagedCliResourceContract) -> Result<(), ManagedResourcesContractError> {
    let label = format!("clis[{}]", cli.name);
    let launch = cli.launch.as_ref().ok_or_else(|| {
        ManagedResourcesContractError::invalid(format!("{label}.launch is required for schemaVersion 3"))
    })?;
    validate_contract_relative_path_field(format!("{label}.launch.program"), &launch.program)?;

    let mut env_names = HashSet::new();
    for (index, entry) in launch.env.iter().enumerate() {
        require_non_empty(format!("{label}.launch.env[{index}].name"), &entry.name)?;
        if entry.name.contains('=') {
            return Err(ManagedResourcesContractError::invalid(format!(
                "{label}.launch.env[{index}].name must not contain '='"
            )));
        }
        if !env_names.insert(entry.name.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate {label}.launch.env name {}",
                entry.name
            )));
        }
        match (&entry.value, &entry.relative_path) {
            (Some(_), None) => {}
            (None, Some(path)) => {
                validate_contract_relative_path_field(format!("{label}.launch.env[{index}].relativePath"), path)?;
            }
            _ => {
                return Err(ManagedResourcesContractError::invalid(format!(
                    "{label}.launch.env[{index}] must contain exactly one of value or relativePath"
                )));
            }
        }
    }
    for (index, entry) in launch.path_entries.iter().enumerate() {
        validate_contract_relative_path_field(format!("{label}.launch.pathEntries[{index}]"), entry)?;
    }

    let mut file_paths = HashSet::new();
    for (index, file) in cli.files.iter().enumerate() {
        validate_contract_relative_path_field(format!("{label}.files[{index}].path"), &file.path)?;
        if !file_paths.insert(file.path.as_str()) {
            return Err(ManagedResourcesContractError::invalid(format!(
                "duplicate {label}.files path {}",
                file.path
            )));
        }
        validate_sha256(format!("{label}.files[{index}].sha256"), &file.sha256)?;
    }
    if !file_paths.contains(launch.program.as_str()) {
        return Err(ManagedResourcesContractError::invalid(format!(
            "{label}.launch.program {} is not declared in files",
            launch.program
        )));
    }
    for key in cli.capabilities.keys() {
        require_non_empty(format!("{label}.capabilities key"), key)?;
    }
    Ok(())
}

fn validate_node_paths(root: &Path, node: &ManagedNodeResourceContract) -> Result<(), ManagedResourcesContractError> {
    let canonical_root = canonical_directory(root, "managed resources root")?;
    let node_root = canonical_directory_under(&canonical_root, &root.join(&node.root), "node root")?;
    canonical_file_under(&node_root, &node_root.join(&node.executable), "node executable")?;
    Ok(())
}

fn validate_cli_entry(
    root: &Path,
    contract: &ManagedResourcesContract,
    cli: &ManagedCliResourceContract,
) -> Result<(), ManagedResourcesContractError> {
    validate_cli_common_schema(contract, cli)?;
    validate_cli_launch_schema(cli)?;

    let canonical_root = canonical_directory(root, "managed resources root")?;
    let cli_root = canonical_directory_under(&canonical_root, &root.join(&cli.root), "CLI root")?;

    if !cli.executable.is_empty() {
        canonical_file_under(&cli_root, &cli_root.join(&cli.executable), "legacy CLI executable")?;
    }
    for required_file in &cli.required_files {
        canonical_file_under(&cli_root, &cli_root.join(required_file), "required CLI file")?;
    }
    for required_directory in &cli.required_directories {
        canonical_directory_under(&cli_root, &cli_root.join(required_directory), "required CLI directory")?;
    }

    let launch = cli.launch.as_ref().expect("launch schema was validated");
    canonical_file_under(&cli_root, &cli_root.join(&launch.program), "launch program")?;

    let mut remaining_files = cli
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.sha256.as_str()))
        .collect::<BTreeMap<_, _>>();
    for entry in WalkDir::new(&cli_root).follow_links(false) {
        let entry = entry.map_err(|error| {
            ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::Io,
                format!("walk managed CLI root {}: {error}", cli_root.display()),
            )
        })?;
        if entry.file_type().is_symlink() {
            return Err(ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::PathEscape,
                format!(
                    "managed CLI contains an unsupported symbolic link: {}",
                    entry.path().display()
                ),
            ));
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = relative_contract_path(&cli_root, entry.path())?;
        let Some(expected) = remaining_files.remove(relative.as_str()) else {
            return Err(ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::HashMismatch,
                format!("managed CLI contains an undeclared file: {relative}"),
            ));
        };
        let actual = hash_file(entry.path())?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(ManagedResourcesContractError::new(
                ManagedResourcesContractErrorCode::HashMismatch,
                format!(
                    "managed CLI file hash mismatch for {}: expected {}, got {actual}",
                    relative, expected
                ),
            ));
        }
    }
    if let Some((missing, _)) = remaining_files.first_key_value() {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("managed CLI declared file is missing: {missing}"),
        ));
    }
    for entry in &launch.env {
        if let Some(relative_path) = &entry.relative_path {
            canonical_existing_under(
                &cli_root,
                &cli_root.join(relative_path),
                "launch environment relativePath",
            )?;
        }
    }
    for entry in &launch.path_entries {
        canonical_directory_under(&cli_root, &cli_root.join(entry), "launch PATH entry")?;
    }
    Ok(())
}

fn validate_sha256(field: impl std::fmt::Display, value: &str) -> Result<(), ManagedResourcesContractError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::InvalidHash,
            format!("{field} must be exactly 64 hexadecimal characters"),
        ));
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, ManagedResourcesContractError> {
    let mut file = File::open(path).map_err(|error| ManagedResourcesContractError::io("open file", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| ManagedResourcesContractError::io("read file", path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, ManagedResourcesContractError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("{label} missing at {}: {error}", path.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("{label} is not a directory: {}", path.display()),
        ));
    }
    Ok(canonical)
}

fn canonical_directory_under(
    parent: &Path,
    path: &Path,
    label: &str,
) -> Result<PathBuf, ManagedResourcesContractError> {
    let canonical = canonical_existing_under(parent, path, label)?;
    if !canonical.is_dir() {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("{label} is not a directory: {}", path.display()),
        ));
    }
    Ok(canonical)
}

fn canonical_file_under(parent: &Path, path: &Path, label: &str) -> Result<PathBuf, ManagedResourcesContractError> {
    let canonical = canonical_existing_under(parent, path, label)?;
    if !canonical.is_file() {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("{label} is not a file: {}", path.display()),
        ));
    }
    Ok(canonical)
}

fn canonical_existing_under(parent: &Path, path: &Path, label: &str) -> Result<PathBuf, ManagedResourcesContractError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::MissingPath,
            format!("{label} missing at {}: {error}", path.display()),
        )
    })?;
    ensure_contained(parent, &canonical, label)?;
    Ok(canonical)
}

fn ensure_contained(parent: &Path, path: &Path, label: &str) -> Result<(), ManagedResourcesContractError> {
    if !path.starts_with(parent) {
        return Err(ManagedResourcesContractError::new(
            ManagedResourcesContractErrorCode::PathEscape,
            format!(
                "{label} escaped managed root: {} is outside {}",
                path.display(),
                parent.display()
            ),
        ));
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

    fn base_contract(runtime_key: &str) -> ManagedResourcesContract {
        ManagedResourcesContract {
            schema_version: MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION,
            runtime_key: runtime_key.into(),
            node: ManagedNodeResourceContract {
                version: "24.11.0".into(),
                root: "node/runtime".into(),
                executable: "node.exe".into(),
            },
            clis: Vec::new(),
        }
    }

    fn materialize_cli(root: &Path, contract: &mut ManagedResourcesContract, name: &str, program: &str) {
        let cli_relative = format!("cli/{name}/1.0.0/{}", contract.runtime_key);
        let cli_root = root.join(&cli_relative);
        let program_path = cli_root.join(program);
        fs::create_dir_all(program_path.parent().expect("program parent")).expect("create program parent");
        fs::write(&program_path, format!("{name} executable")).expect("write program");
        let files = collect_file_hashes(&cli_root).expect("hash CLI");
        contract.clis.push(ManagedCliResourceContract {
            name: name.into(),
            version: "1.0.0".into(),
            root: cli_relative,
            platform_directory: contract.runtime_key.clone(),
            executable: program.into(),
            required_files: Vec::new(),
            required_directories: Vec::new(),
            launch: Some(ManagedCliLaunchContract {
                program: program.into(),
                args_prefix: vec!["acp".into()],
                env: Vec::new(),
                path_entries: Vec::new(),
            }),
            files,
            capabilities: BTreeMap::from([("browser".into(), "not-installed".into())]),
        });
    }

    fn materialize_node(root: &Path, contract: &ManagedResourcesContract) {
        let node_root = root.join(&contract.node.root);
        fs::create_dir_all(&node_root).expect("create node");
        fs::write(node_root.join(&contract.node.executable), b"node").expect("write node");
    }

    #[test]
    fn contract_serializes_v3_camel_case_schema() {
        let mut contract = base_contract("win32-x64");
        let temp = tempfile::tempdir().expect("tempdir");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        let value = serde_json::to_value(&contract).expect("serialize");

        assert_eq!(value["schemaVersion"], 3);
        assert_eq!(value["runtimeKey"], "win32-x64");
        assert_eq!(value["clis"][0]["target"], "win32-x64");
        assert!(value["clis"][0].get("platformDirectory").is_none());
        assert_eq!(value["clis"][0]["launch"]["argsPrefix"][0], "acp");
        assert_eq!(value["clis"][0]["capabilities"]["browser"], "not-installed");
        assert!(value.get("schema_version").is_none());
    }

    #[test]
    fn read_contract_accepts_legal_v2_for_compatibility() {
        let temp = tempfile::tempdir().expect("tempdir");
        let value = serde_json::json!({
            "schemaVersion": 2,
            "runtimeKey": "win32-x64",
            "node": {"version": "24.11.0", "root": "node/runtime", "executable": "node.exe"},
            "clis": [
                {
                    "name": "claude", "version": "1", "root": "cli/claude",
                    "platformDirectory": "win32-x64", "executable": "claude.exe"
                },
                {
                    "name": "codex", "version": "1", "root": "cli/codex",
                    "platformDirectory": "win32-x64", "executable": "codex.exe"
                }
            ]
        });
        fs::write(
            temp.path().join(MANAGED_RESOURCES_CONTRACT_FILE),
            serde_json::to_vec(&value).expect("serialize"),
        )
        .expect("write");

        let contract = read_contract(temp.path()).expect("read v2");
        assert_eq!(contract.schema_version, 2);
        let error = validate_cli(temp.path(), &contract, "claude").expect_err("v2 launch must fail closed");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::UnsupportedSchema);
    }

    #[test]
    fn validate_cli_rejects_duplicate_file_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        let duplicate = contract.clis[0].files[0].clone();
        contract.clis[0].files.push(duplicate);

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("duplicate must fail");
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn validate_cli_requires_canonical_sha256_and_exclusive_env_value() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        contract.clis[0].files[0].sha256 = "not-a-sha256".into();

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("invalid hash must fail");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::InvalidHash);

        contract.clis[0].files = collect_file_hashes(&temp.path().join(&contract.clis[0].root)).expect("rehash");
        contract.clis[0].launch.as_mut().expect("launch").env = vec![ManagedCliLaunchEnvContract {
            name: "HERMES_HOME".into(),
            value: Some("literal".into()),
            relative_path: Some("state".into()),
        }];
        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("ambiguous env must fail");
        assert!(error.to_string().contains("exactly one"));
    }

    #[test]
    fn validate_cli_rejects_tampered_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        fs::write(
            temp.path().join(&contract.clis[0].root).join("python/python.exe"),
            b"tampered",
        )
        .expect("tamper");

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("tamper must fail");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::HashMismatch);
    }

    #[test]
    fn validate_cli_rejects_missing_hashed_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        fs::remove_file(temp.path().join(&contract.clis[0].root).join("python/python.exe")).expect("remove");

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("missing file must fail");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::MissingPath);
    }

    #[test]
    fn validate_cli_rejects_undeclared_file_injection() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        fs::write(
            temp.path().join(&contract.clis[0].root).join("python/injected.py"),
            b"raise SystemExit('injected')",
        )
        .expect("inject");

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("undeclared file must fail");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::HashMismatch);
        assert!(error.to_string().contains("undeclared file"));
    }

    #[test]
    fn validate_cli_rejects_lexical_path_escape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");
        contract.clis[0].launch.as_mut().expect("launch").program = "../outside.exe".into();

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("escape must fail");
        assert!(error.to_string().contains("invalid relative contract path"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_cli_rejects_canonical_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        fs::write(outside.join("python"), b"outside").expect("outside program");

        let mut contract = base_contract("linux-x64");
        let cli_root = temp.path().join("managed/cli/hermes");
        fs::create_dir_all(&cli_root).expect("cli root");
        symlink(&outside, cli_root.join("escaped")).expect("symlink");
        contract.clis.push(ManagedCliResourceContract {
            name: "hermes".into(),
            version: "1".into(),
            root: "managed/cli/hermes".into(),
            platform_directory: "linux-x64".into(),
            executable: String::new(),
            required_files: Vec::new(),
            required_directories: Vec::new(),
            launch: Some(ManagedCliLaunchContract {
                program: "escaped/python".into(),
                args_prefix: Vec::new(),
                env: Vec::new(),
                path_entries: Vec::new(),
            }),
            files: vec![ManagedCliFileContract {
                path: "escaped/python".into(),
                sha256: hash_file(&outside.join("python")).expect("hash"),
            }],
            capabilities: BTreeMap::new(),
        });

        let error = validate_cli(temp.path(), &contract, "hermes").expect_err("symlink escape must fail");
        assert_eq!(error.code(), ManagedResourcesContractErrorCode::PathEscape);
    }

    #[test]
    fn collect_file_hashes_is_sorted_and_stable_with_unicode_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("带 空格").join("Hermes 运行时");
        fs::create_dir_all(root.join("python")).expect("mkdir");
        fs::write(root.join("z.txt"), b"z").expect("write z");
        fs::write(root.join("python").join("解释器.exe"), b"python").expect("write python");

        let first = collect_file_hashes(&root).expect("first");
        let second = collect_file_hashes(&root).expect("second");

        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["python/解释器.exe", "z.txt"]
        );
    }

    #[test]
    fn validate_contract_checks_node_and_all_v3_clis() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = base_contract("win32-x64");
        materialize_node(temp.path(), &contract);
        materialize_cli(temp.path(), &mut contract, "hermes", "python/python.exe");

        validate_contract(temp.path(), &contract).expect("valid contract");
    }
}
