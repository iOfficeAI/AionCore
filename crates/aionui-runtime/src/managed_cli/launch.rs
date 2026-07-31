use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::managed_cli::current_runtime_key;
use crate::managed_resources::{self, ManagedResourcesMode};
use crate::managed_resources_contract::{
    MANAGED_RESOURCES_CONTRACT_FILE, ManagedCliResourceContract, ManagedResourcesContract,
    ManagedResourcesContractError, read_contract, validate_cli,
};
use crate::{ResolvedCommand, resolve_command_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCliLaunchSource {
    UserOverride,
    Managed,
    Path,
}

impl ManagedCliLaunchSource {
    const fn log_value(self) -> &'static str {
        match self {
            Self::UserOverride => "user_override",
            Self::Managed => "managed",
            Self::Path => "path",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCliLaunchPlan {
    pub command: ResolvedCommand,
    pub source: ManagedCliLaunchSource,
    pub version: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedCliLaunchError {
    #[error("CLI {name} command {command:?} was not found")]
    Missing { name: String, command: String },
    #[error("managed CLI {name} is broken at {stage} ({code}): {detail}")]
    Broken {
        name: String,
        stage: &'static str,
        code: &'static str,
        detail: String,
    },
}

/// Resolve an agent CLI with the fixed priority:
///
/// user command override > bundled manifest > PATH.
///
/// A declared managed entry fails closed. PATH is considered only when no
/// manifest exists or the readable manifest does not declare `name`.
pub fn resolve_managed_cli_launch(
    name: &str,
    configured_command: &str,
    configured_args: &[String],
    has_command_override: bool,
) -> Result<ManagedCliLaunchPlan, ManagedCliLaunchError> {
    let managed_root = matches!(
        managed_resources::managed_resources_mode(),
        ManagedResourcesMode::Bundled
    )
    .then(managed_resources::bundled_root_candidate)
    .flatten();
    resolve_with(
        name,
        configured_command,
        configured_args,
        has_command_override,
        managed_root.as_deref(),
        current_runtime_key(),
        resolve_command_path,
    )
}

fn resolve_with<F>(
    name: &str,
    configured_command: &str,
    configured_args: &[String],
    has_command_override: bool,
    managed_root: Option<&Path>,
    runtime_key: Option<&str>,
    mut path_resolver: F,
) -> Result<ManagedCliLaunchPlan, ManagedCliLaunchError>
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    let lookup_command = if configured_command.is_empty() {
        name
    } else {
        configured_command
    };

    if has_command_override {
        return resolve_external(
            name,
            lookup_command,
            configured_args,
            ManagedCliLaunchSource::UserOverride,
            &mut path_resolver,
        );
    }

    if let Some(root) = managed_root {
        let manifest_path = root.join(MANAGED_RESOURCES_CONTRACT_FILE);
        match manifest_path.try_exists() {
            Ok(false) => {}
            Ok(true) => {
                let contract =
                    read_contract(root).map_err(|error| broken_contract(name, None, None, "read_manifest", &error))?;
                if let Some(cli) = contract.clis.iter().find(|cli| cli.name == name) {
                    if contract.schema_version != 3 {
                        return Err(broken(
                            name,
                            Some(contract.schema_version),
                            Some(&cli.version),
                            "validate_manifest",
                            "missing_launch",
                            format!(
                                "schemaVersion {} cannot provide a trusted launch contract",
                                contract.schema_version
                            ),
                        ));
                    }
                    let Some(runtime_key) = runtime_key else {
                        return Err(broken(
                            name,
                            Some(contract.schema_version),
                            Some(&cli.version),
                            "select_runtime",
                            "unsupported_runtime",
                            "the current operating system and architecture have no runtime key".into(),
                        ));
                    };
                    if contract.runtime_key != runtime_key {
                        return Err(broken(
                            name,
                            Some(contract.schema_version),
                            Some(&cli.version),
                            "select_runtime",
                            "runtime_key_mismatch",
                            format!(
                                "manifest runtimeKey {} does not match current runtime {runtime_key}",
                                contract.runtime_key
                            ),
                        ));
                    }
                    validate_cli(root, &contract, name).map_err(|error| {
                        broken_contract(
                            name,
                            Some(contract.schema_version),
                            Some(&cli.version),
                            "validate_cli",
                            &error,
                        )
                    })?;
                    return managed_plan(root, &contract, cli).map_err(|(stage, code, detail)| {
                        broken(
                            name,
                            Some(contract.schema_version),
                            Some(&cli.version),
                            stage,
                            code,
                            detail,
                        )
                    });
                }
            }
            Err(error) => {
                return Err(broken(
                    name,
                    None,
                    None,
                    "inspect_manifest",
                    "io",
                    format!("inspect {}: {error}", manifest_path.display()),
                ));
            }
        }
    }

    resolve_external(
        name,
        lookup_command,
        configured_args,
        ManagedCliLaunchSource::Path,
        &mut path_resolver,
    )
}

fn resolve_external<F>(
    name: &str,
    command: &str,
    configured_args: &[String],
    source: ManagedCliLaunchSource,
    path_resolver: &mut F,
) -> Result<ManagedCliLaunchPlan, ManagedCliLaunchError>
where
    F: FnMut(&str) -> Option<PathBuf>,
{
    let Some(program) = path_resolver(command) else {
        tracing::warn!(
            source = source.log_value(),
            version = "<none>",
            schema = 0_u8,
            stage = "resolve_command",
            code = "missing",
            "managed CLI launch resolution failed"
        );
        return Err(ManagedCliLaunchError::Missing {
            name: name.to_owned(),
            command: command.to_owned(),
        });
    };
    let plan = ManagedCliLaunchPlan {
        command: ResolvedCommand {
            program,
            args_prefix: configured_args.iter().map(OsString::from).collect(),
            env: Vec::new(),
        },
        source,
        version: None,
    };
    tracing::info!(
        source = source.log_value(),
        version = "<none>",
        schema = 0_u8,
        stage = "resolve_command",
        code = "ready",
        "managed CLI launch resolved"
    );
    Ok(plan)
}

type ManagedPlanBuildError = (&'static str, &'static str, String);

fn managed_plan(
    root: &Path,
    contract: &ManagedResourcesContract,
    cli: &ManagedCliResourceContract,
) -> Result<ManagedCliLaunchPlan, ManagedPlanBuildError> {
    let cli_root = std::fs::canonicalize(root.join(&cli.root))
        .map_err(|error| ("build_plan", "canonicalize", error.to_string()))?;
    let launch = cli
        .launch
        .as_ref()
        .ok_or_else(|| ("build_plan", "missing_launch", "launch is missing".into()))?;
    let program = std::fs::canonicalize(cli_root.join(&launch.program))
        .map_err(|error| ("build_plan", "canonicalize", error.to_string()))?;

    let mut env = Vec::with_capacity(launch.env.len() + usize::from(!launch.path_entries.is_empty()));
    for entry in &launch.env {
        let value = match (&entry.value, &entry.relative_path) {
            (Some(value), None) => OsString::from(value),
            (None, Some(relative_path)) => std::fs::canonicalize(cli_root.join(relative_path))
                .map_err(|error| ("build_env", "canonicalize", error.to_string()))?
                .into_os_string(),
            _ => {
                return Err((
                    "build_env",
                    "invalid_env",
                    format!("environment entry {} is not exclusive", entry.name),
                ));
            }
        };
        env.push((OsString::from(&entry.name), value));
    }

    if !launch.path_entries.is_empty() {
        let mut paths = Vec::new();
        for entry in &launch.path_entries {
            paths.push(
                std::fs::canonicalize(cli_root.join(entry))
                    .map_err(|error| ("build_path", "canonicalize", error.to_string()))?,
            );
        }
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        let value = std::env::join_paths(paths).map_err(|error| ("build_path", "join_path", error.to_string()))?;
        remove_environment_key(&mut env, OsStr::new("PATH"));
        env.push((OsString::from("PATH"), value));
    }

    let plan = ManagedCliLaunchPlan {
        command: ResolvedCommand {
            program,
            args_prefix: launch.args_prefix.iter().map(OsString::from).collect(),
            env,
        },
        source: ManagedCliLaunchSource::Managed,
        version: Some(cli.version.clone()),
    };
    tracing::info!(
        source = ManagedCliLaunchSource::Managed.log_value(),
        version = cli.version.as_str(),
        schema = contract.schema_version,
        stage = "build_plan",
        code = "ready",
        "managed CLI launch resolved"
    );
    Ok(plan)
}

fn remove_environment_key(env: &mut Vec<(OsString, OsString)>, name: &OsStr) {
    #[cfg(windows)]
    env.retain(|(key, _)| !key.to_string_lossy().eq_ignore_ascii_case(&name.to_string_lossy()));
    #[cfg(not(windows))]
    env.retain(|(key, _)| key != name);
}

fn broken_contract(
    name: &str,
    schema: Option<u8>,
    version: Option<&str>,
    stage: &'static str,
    error: &ManagedResourcesContractError,
) -> ManagedCliLaunchError {
    broken(name, schema, version, stage, error.code().as_str(), error.to_string())
}

fn broken(
    name: &str,
    schema: Option<u8>,
    version: Option<&str>,
    stage: &'static str,
    code: &'static str,
    detail: String,
) -> ManagedCliLaunchError {
    tracing::warn!(
        source = ManagedCliLaunchSource::Managed.log_value(),
        version = version.unwrap_or("<unknown>"),
        schema = schema.unwrap_or(0),
        stage,
        code,
        "managed CLI launch resolution failed"
    );
    ManagedCliLaunchError::Broken {
        name: name.to_owned(),
        stage,
        code,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_resources_contract::{
        MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION, ManagedCliFileContract, ManagedCliLaunchContract,
        ManagedCliLaunchEnvContract, ManagedNodeResourceContract, collect_file_hashes,
    };
    use std::collections::BTreeMap;
    use std::fs;

    fn fake_path(root: &Path, name: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, b"external").expect("write external command");
        path
    }

    fn write_json_contract(root: &Path, contract: &ManagedResourcesContract) {
        fs::write(
            root.join(MANAGED_RESOURCES_CONTRACT_FILE),
            serde_json::to_vec_pretty(contract).expect("serialize contract"),
        )
        .expect("write contract");
    }

    fn valid_managed_contract(root: &Path, runtime_key: &str) -> ManagedResourcesContract {
        let cli_root = root.join("cli").join("Hermes 运行时").join("1.2.3").join(runtime_key);
        fs::create_dir_all(cli_root.join("python")).expect("python dir");
        fs::create_dir_all(cli_root.join("git").join("bin")).expect("git bin");
        fs::create_dir_all(cli_root.join("state").join("默认")).expect("state dir");
        fs::write(cli_root.join("python").join("python.exe"), b"python").expect("python");
        fs::write(cli_root.join("git").join("bin").join("bash.exe"), b"bash").expect("bash");
        let files = collect_file_hashes(&cli_root).expect("hash files");

        ManagedResourcesContract {
            schema_version: MANAGED_RESOURCES_CONTRACT_SCHEMA_VERSION,
            runtime_key: runtime_key.into(),
            node: ManagedNodeResourceContract {
                version: "24.11.0".into(),
                root: "node/runtime".into(),
                executable: "node.exe".into(),
            },
            clis: vec![ManagedCliResourceContract {
                name: "hermes".into(),
                version: "1.2.3".into(),
                root: format!("cli/Hermes 运行时/1.2.3/{runtime_key}"),
                platform_directory: runtime_key.into(),
                executable: String::new(),
                required_files: Vec::new(),
                required_directories: Vec::new(),
                launch: Some(ManagedCliLaunchContract {
                    program: "python/python.exe".into(),
                    args_prefix: vec!["-m".into(), "acp_adapter".into()],
                    env: vec![
                        ManagedCliLaunchEnvContract {
                            name: "STATIC_MODE".into(),
                            value: Some("offline".into()),
                            relative_path: None,
                        },
                        ManagedCliLaunchEnvContract {
                            name: "HERMES_GIT_BASH_PATH".into(),
                            value: None,
                            relative_path: Some("git/bin/bash.exe".into()),
                        },
                        ManagedCliLaunchEnvContract {
                            name: "HERMES_HOME".into(),
                            value: None,
                            relative_path: Some("state/默认".into()),
                        },
                    ],
                    path_entries: vec!["git/bin".into()],
                }),
                files,
                capabilities: BTreeMap::from([("browser".into(), "not-installed".into())]),
            }],
        }
    }

    fn resolve_test(
        root: Option<&Path>,
        runtime_key: &str,
        configured_command: &str,
        configured_args: &[String],
        has_override: bool,
        external: Option<PathBuf>,
    ) -> Result<ManagedCliLaunchPlan, ManagedCliLaunchError> {
        resolve_with(
            "hermes",
            configured_command,
            configured_args,
            has_override,
            root,
            Some(runtime_key),
            |_| external.clone(),
        )
    }

    #[test]
    fn user_override_bypasses_malformed_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join(MANAGED_RESOURCES_CONTRACT_FILE), b"{bad").expect("bad manifest");
        let override_path = fake_path(temp.path(), "custom-hermes");
        let configured_args = vec!["acp".into(), "--custom".into()];

        let plan = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "custom-hermes",
            &configured_args,
            true,
            Some(override_path.clone()),
        )
        .expect("override");

        assert_eq!(plan.source, ManagedCliLaunchSource::UserOverride);
        assert_eq!(plan.command.program, override_path);
        assert_eq!(
            plan.command.args_prefix,
            vec![OsString::from("acp"), OsString::from("--custom")]
        );
    }

    #[test]
    fn managed_manifest_beats_path_and_ignores_configured_args() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = valid_managed_contract(temp.path(), "win32-x64");
        write_json_contract(temp.path(), &contract);
        let path_command = fake_path(temp.path(), "path-hermes");

        let plan = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &["legacy-acp".into()],
            false,
            Some(path_command),
        )
        .expect("managed");

        assert_eq!(plan.source, ManagedCliLaunchSource::Managed);
        assert_eq!(plan.version.as_deref(), Some("1.2.3"));
        assert_eq!(
            plan.command.args_prefix,
            vec![OsString::from("-m"), OsString::from("acp_adapter")]
        );
        assert!(plan.command.program.ends_with(Path::new("python").join("python.exe")));
    }

    #[test]
    fn absent_manifest_falls_back_to_path_with_configured_args() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path_command = fake_path(temp.path(), "path-hermes");

        let plan = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &["acp".into()],
            false,
            Some(path_command.clone()),
        )
        .expect("PATH fallback");

        assert_eq!(plan.source, ManagedCliLaunchSource::Path);
        assert_eq!(plan.command.program, path_command);
        assert_eq!(plan.command.args_prefix, vec![OsString::from("acp")]);
    }

    #[test]
    fn v3_without_target_entry_falls_back_to_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = valid_managed_contract(temp.path(), "win32-x64");
        contract.clis[0].name = "another-agent".into();
        write_json_contract(temp.path(), &contract);
        let path_command = fake_path(temp.path(), "path-hermes");

        let plan = resolve_test(Some(temp.path()), "win32-x64", "hermes", &[], false, Some(path_command))
            .expect("PATH fallback");
        assert_eq!(plan.source, ManagedCliLaunchSource::Path);
    }

    #[test]
    fn malformed_manifest_is_broken_and_does_not_fall_back() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join(MANAGED_RESOURCES_CONTRACT_FILE), b"{bad").expect("bad manifest");

        let error = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &[],
            false,
            Some(fake_path(temp.path(), "path-hermes")),
        )
        .expect_err("malformed must fail closed");

        assert!(matches!(
            error,
            ManagedCliLaunchError::Broken {
                stage: "read_manifest",
                code: "malformed_json",
                ..
            }
        ));
    }

    #[test]
    fn v2_without_target_entry_falls_back_but_declared_target_is_broken() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut value = serde_json::json!({
            "schemaVersion": 2,
            "runtimeKey": "win32-x64",
            "node": {"version": "24", "root": "node/runtime", "executable": "node.exe"},
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
        .expect("write v2");
        let external = fake_path(temp.path(), "path-hermes");
        let plan = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &[],
            false,
            Some(external.clone()),
        )
        .expect("v2 not declared");
        assert_eq!(plan.source, ManagedCliLaunchSource::Path);

        value["clis"].as_array_mut().expect("clis").push(serde_json::json!({
            "name": "hermes", "version": "1", "root": "cli/hermes",
            "platformDirectory": "win32-x64", "executable": "hermes.exe"
        }));
        fs::write(
            temp.path().join(MANAGED_RESOURCES_CONTRACT_FILE),
            serde_json::to_vec(&value).expect("serialize"),
        )
        .expect("write v2");
        let error = resolve_test(Some(temp.path()), "win32-x64", "hermes", &[], false, Some(external))
            .expect_err("declared v2 target must fail");
        assert!(matches!(
            error,
            ManagedCliLaunchError::Broken {
                code: "missing_launch",
                ..
            }
        ));
    }

    #[test]
    fn declared_tampered_or_missing_managed_program_is_broken() {
        for remove in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let contract = valid_managed_contract(temp.path(), "win32-x64");
            write_json_contract(temp.path(), &contract);
            let program = temp
                .path()
                .join(&contract.clis[0].root)
                .join("python")
                .join("python.exe");
            if remove {
                fs::remove_file(program).expect("remove program");
            } else {
                fs::write(program, b"tampered").expect("tamper program");
            }

            let error = resolve_test(
                Some(temp.path()),
                "win32-x64",
                "hermes",
                &[],
                false,
                Some(fake_path(temp.path(), "path-hermes")),
            )
            .expect_err("declared broken CLI must not fall back");
            assert!(matches!(error, ManagedCliLaunchError::Broken { .. }));
        }
    }

    #[test]
    fn runtime_key_mismatch_is_broken() {
        let temp = tempfile::tempdir().expect("tempdir");
        let contract = valid_managed_contract(temp.path(), "linux-x64");
        write_json_contract(temp.path(), &contract);

        let error = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &[],
            false,
            Some(fake_path(temp.path(), "path-hermes")),
        )
        .expect_err("wrong target must fail");
        assert!(matches!(
            error,
            ManagedCliLaunchError::Broken {
                code: "runtime_key_mismatch",
                ..
            }
        ));
    }

    #[test]
    fn managed_plan_absolutizes_relative_env_and_prepends_path() {
        let temp = tempfile::Builder::new()
            .prefix("aion 路径 with spaces ")
            .tempdir()
            .expect("tempdir");
        let contract = valid_managed_contract(temp.path(), "win32-x64");
        write_json_contract(temp.path(), &contract);

        let plan = resolve_test(
            Some(temp.path()),
            "win32-x64",
            "hermes",
            &["must-not-appear".into()],
            false,
            None,
        )
        .expect("managed plan");

        let env_value = |name: &str| {
            plan.command
                .env
                .iter()
                .find(|(key, _)| key == OsStr::new(name))
                .map(|(_, value)| value.clone())
                .expect("environment key")
        };
        assert_eq!(env_value("STATIC_MODE"), OsString::from("offline"));
        assert!(
            PathBuf::from(env_value("HERMES_GIT_BASH_PATH")).ends_with(Path::new("git").join("bin").join("bash.exe"))
        );
        assert!(PathBuf::from(env_value("HERMES_HOME")).ends_with(Path::new("state").join("默认")));
        let path_entries = std::env::split_paths(&env_value("PATH")).collect::<Vec<_>>();
        assert!(
            path_entries
                .first()
                .expect("first PATH entry")
                .ends_with(Path::new("git").join("bin"))
        );
    }

    #[test]
    fn launch_program_must_be_hash_listed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut contract = valid_managed_contract(temp.path(), "win32-x64");
        contract.clis[0].files = vec![ManagedCliFileContract {
            path: "git/bin/bash.exe".into(),
            sha256: contract.clis[0]
                .files
                .iter()
                .find(|file| file.path == "git/bin/bash.exe")
                .expect("bash hash")
                .sha256
                .clone(),
        }];
        write_json_contract(temp.path(), &contract);

        let error =
            resolve_test(Some(temp.path()), "win32-x64", "hermes", &[], false, None).expect_err("unlisted program");
        assert!(matches!(
            error,
            ManagedCliLaunchError::Broken {
                stage: "validate_cli",
                ..
            }
        ));
    }
}
