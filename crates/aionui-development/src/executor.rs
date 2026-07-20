use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;

use aionui_db::models::{DevelopmentPolicyRow, ProjectRuntimeProfileRow};
use aionui_runtime::{Builder, ProcessLeaseSpec};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use zeroize::Zeroize;

use crate::DevelopmentError;
use crate::operations::redact_sensitive;
use crate::resources::{ResourceLeaseCoordinator, ResourceLeaseInput};

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct CommandExecutionInput<'a> {
    pub execution_id: &'a str,
    pub run_id: &'a str,
    pub command: &'a str,
    pub working_directory: &'a Path,
    pub timeout_seconds: i64,
    pub policy: &'a DevelopmentPolicyRow,
    pub runtime_profile: Option<&'a ProjectRuntimeProfileRow>,
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionCommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandExecutionPlan {
    pub execution_id: String,
    pub isolation_mode: String,
    pub environment_id: String,
    pub steps: Vec<ExecutionCommandSpec>,
    pub cleanup: Option<ExecutionCommandSpec>,
    pub resources: Vec<PlannedExecutionResource>,
    #[serde(skip)]
    secret_values: Vec<String>,
}

impl Drop for CommandExecutionPlan {
    fn drop(&mut self) {
        for value in &mut self.secret_values {
            value.zeroize();
        }
        for step in &mut self.steps {
            step.environment.values_mut().for_each(Zeroize::zeroize);
        }
        if let Some(cleanup) = &mut self.cleanup {
            cleanup.environment.values_mut().for_each(Zeroize::zeroize);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedExecutionResource {
    pub resource_kind: String,
    pub resource_identifier: String,
    pub cleanup_order: i64,
}

pub(crate) struct ManagedExecutionContext<'a> {
    pub user_id: &'a str,
    pub project_id: &'a str,
    pub run_id: &'a str,
    pub task_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub gate_id: Option<&'a str>,
    pub resources: &'a ResourceLeaseCoordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandExecutionOutput {
    pub status: String,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub isolation_mode: String,
    pub execution_id: String,
}

pub fn build_execution_plan(input: &CommandExecutionInput<'_>) -> Result<CommandExecutionPlan, DevelopmentError> {
    if input.timeout_seconds <= 0 {
        return Err(DevelopmentError::BadRequest("command timeout must be positive".into()));
    }
    if input.command.trim().is_empty() {
        return Err(DevelopmentError::BadRequest("command must not be empty".into()));
    }
    let workspace = input
        .working_directory
        .canonicalize()
        .map_err(|error| DevelopmentError::BadRequest(format!("working directory is unavailable: {error}")))?;
    if !workspace.is_dir() {
        return Err(DevelopmentError::BadRequest(
            "working directory is not a directory".into(),
        ));
    }
    let (secret_environment, secret_values) = selected_secret_environment(input)?;
    let execution_name = safe_execution_name(input.execution_id);

    match input.policy.isolation_mode.as_str() {
        "host" => Ok(CommandExecutionPlan {
            execution_id: input.execution_id.into(),
            isolation_mode: "host".into(),
            environment_id: "host:local".into(),
            steps: vec![host_command(input.command, &workspace, &secret_environment)],
            cleanup: None,
            resources: Vec::new(),
            secret_values,
        }),
        "docker" => {
            let image = input
                .policy
                .container_image
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DevelopmentError::BadRequest("docker policy requires a container image".into()))?;
            let container_name = format!("aion-{execution_name}");
            let mut args = vec![
                "run".into(),
                "--rm".into(),
                "--init".into(),
                "--name".into(),
                container_name.clone(),
                "--label".into(),
                format!("aion.run={}", safe_execution_name(input.run_id)),
                "--security-opt".into(),
                "no-new-privileges".into(),
                "--cap-drop".into(),
                "ALL".into(),
                "--pids-limit".into(),
                input.policy.container_pids_limit.to_string(),
                "--cpus".into(),
                format!("{:.3}", input.policy.container_cpu_millis as f64 / 1000.0),
                "--memory".into(),
                format!("{}m", input.policy.container_memory_mb),
                "--network".into(),
                input.policy.network_mode.clone(),
                "--workdir".into(),
                "/workspace".into(),
                "--volume".into(),
                format!("{}:/workspace:rw", workspace.display()),
            ];
            for key in secret_environment.keys() {
                args.push("--env".into());
                args.push(key.clone());
            }
            args.extend([image.into(), "sh".into(), "-lc".into(), input.command.into()]);
            Ok(CommandExecutionPlan {
                execution_id: input.execution_id.into(),
                isolation_mode: "docker".into(),
                environment_id: format!("docker:{container_name}"),
                steps: vec![ExecutionCommandSpec {
                    program: "docker".into(),
                    args,
                    working_directory: workspace.clone(),
                    environment: secret_environment,
                }],
                cleanup: Some(ExecutionCommandSpec {
                    program: "docker".into(),
                    args: vec!["rm".into(), "--force".into(), container_name.clone()],
                    working_directory: workspace,
                    environment: BTreeMap::new(),
                }),
                resources: vec![PlannedExecutionResource {
                    resource_kind: "container".into(),
                    resource_identifier: container_name,
                    cleanup_order: 40,
                }],
                secret_values,
            })
        }
        "devcontainer" => {
            let relative_config = input
                .policy
                .devcontainer_config_path
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DevelopmentError::BadRequest("devcontainer policy requires a config path".into()))?;
            let relative_path = Path::new(relative_config);
            if relative_path.is_absolute()
                || relative_path
                    .components()
                    .any(|component| component == Component::ParentDir)
            {
                return Err(DevelopmentError::BadRequest(
                    "devcontainer config must stay inside the project".into(),
                ));
            }
            let config = workspace.join(relative_path).canonicalize().map_err(|error| {
                DevelopmentError::BadRequest(format!("devcontainer config is unavailable: {error}"))
            })?;
            if !config.starts_with(&workspace) || config.extension().and_then(|value| value.to_str()) != Some("json") {
                return Err(DevelopmentError::BadRequest(
                    "devcontainer config must be a JSON file inside the project".into(),
                ));
            }
            let common = vec![
                "--workspace-folder".into(),
                workspace.to_string_lossy().into_owned(),
                "--config".into(),
                config.to_string_lossy().into_owned(),
            ];
            let mut up = vec!["up".into()];
            up.extend(common.clone());
            let mut exec = vec!["exec".into()];
            exec.extend(common);
            exec.extend(["sh".into(), "-lc".into(), input.command.into()]);
            let service_identifier = workspace.to_string_lossy().into_owned();
            Ok(CommandExecutionPlan {
                execution_id: input.execution_id.into(),
                isolation_mode: "devcontainer".into(),
                environment_id: format!("devcontainer:{execution_name}"),
                steps: vec![
                    ExecutionCommandSpec {
                        program: "devcontainer".into(),
                        args: up,
                        working_directory: workspace.clone(),
                        environment: BTreeMap::new(),
                    },
                    ExecutionCommandSpec {
                        program: "devcontainer".into(),
                        args: exec,
                        working_directory: workspace,
                        environment: BTreeMap::new(),
                    },
                ],
                cleanup: None,
                resources: vec![PlannedExecutionResource {
                    resource_kind: "service".into(),
                    resource_identifier: service_identifier,
                    cleanup_order: 30,
                }],
                secret_values,
            })
        }
        other => Err(DevelopmentError::BadRequest(format!(
            "unsupported isolation mode: {other}"
        ))),
    }
}

pub async fn execute_command(mut input: CommandExecutionInput<'_>) -> Result<CommandExecutionOutput, DevelopmentError> {
    let timeout_seconds = input.timeout_seconds;
    let plan = build_execution_plan(&input)?;
    input.environment.values_mut().for_each(Zeroize::zeroize);
    execute_plan(&plan, timeout_seconds, None).await
}

pub(crate) async fn execute_plan(
    plan: &CommandExecutionPlan,
    timeout_seconds: i64,
    context: Option<&ManagedExecutionContext<'_>>,
) -> Result<CommandExecutionOutput, DevelopmentError> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut final_status = "passed".to_owned();
    let mut final_exit_code = Some(0);
    for step in &plan.steps {
        let output = execute_step(step, plan, timeout_seconds, context).await?;
        append_bounded(&mut stdout, &output.stdout);
        append_bounded(&mut stderr, &output.stderr);
        final_status = output.status;
        final_exit_code = output.exit_code;
        if final_status != "passed" {
            if final_status == "timed_out"
                && let Some(cleanup) = &plan.cleanup
            {
                let _ = execute_step(cleanup, plan, 15, context).await;
            }
            break;
        }
    }
    Ok(CommandExecutionOutput {
        status: final_status,
        exit_code: final_exit_code,
        stdout: redact_sensitive(&stdout, &plan.secret_values),
        stderr: redact_sensitive(&stderr, &plan.secret_values),
        isolation_mode: plan.isolation_mode.clone(),
        execution_id: plan.execution_id.clone(),
    })
}

struct StepOutput {
    status: String,
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
}

async fn execute_step(
    spec: &ExecutionCommandSpec,
    plan: &CommandExecutionPlan,
    timeout_seconds: i64,
    context: Option<&ManagedExecutionContext<'_>>,
) -> Result<StepOutput, DevelopmentError> {
    let mut command = Builder::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.working_directory)
        .envs(&spec.environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn_leased(ProcessLeaseSpec::new(
            format!("{}:process", plan.execution_id),
            &plan.environment_id,
            std::time::Duration::from_secs(timeout_seconds.max(1) as u64),
        ))
        .map_err(|error| DevelopmentError::BadRequest(format!("{} is unavailable: {error}", spec.program)))?;
    let process_resource = match context {
        Some(context) => {
            let pid = child
                .id()
                .ok_or_else(|| DevelopmentError::Internal("spawned process has no pid".into()))?;
            let created = context
                .resources
                .create(ResourceLeaseInput {
                    user_id: context.user_id.into(),
                    project_id: context.project_id.into(),
                    run_id: context.run_id.into(),
                    task_id: context.task_id.map(str::to_owned),
                    turn_id: context.turn_id.map(str::to_owned),
                    gate_id: context.gate_id.map(str::to_owned),
                    environment_id: plan.environment_id.clone(),
                    environment_kind: plan.isolation_mode.clone(),
                    resource_kind: "process".into(),
                    resource_identifier: pid.to_string(),
                    cleanup_order: 20,
                    ttl_ms: timeout_seconds.max(1).saturating_mul(1000),
                })
                .await;
            match created {
                Ok(resource) => Some(resource),
                Err(error) => {
                    let _ = child.terminate_tree().await;
                    return Err(error);
                }
            }
        }
        None => None,
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.terminate_tree().await;
            return Err(DevelopmentError::Internal("stdout pipe missing".into()));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.terminate_tree().await;
            return Err(DevelopmentError::Internal("stderr pipe missing".into()));
        }
    };
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let started = std::time::Instant::now();
    let mut last_persisted_heartbeat = started;
    let (status, exit_code): (String, Option<i64>) = loop {
        if let Some(status) = child.try_wait()? {
            break (
                if status.success() { "passed" } else { "failed" }.into(),
                status.code().map(i64::from),
            );
        }
        if started.elapsed() >= std::time::Duration::from_secs(timeout_seconds.max(1) as u64) {
            child.terminate_tree().await?;
            break ("timed_out".into(), None);
        }
        child.lease().heartbeat();
        if last_persisted_heartbeat.elapsed() >= std::time::Duration::from_secs(5)
            && let (Some(context), Some(resource)) = (context, process_resource.as_ref())
        {
            if let Err(error) = context
                .resources
                .heartbeat(&resource.id, timeout_seconds.max(1).saturating_mul(1000))
                .await
            {
                let _ = child.terminate_tree().await;
                return Err(error);
            }
            last_persisted_heartbeat = std::time::Instant::now();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    if let (Some(context), Some(resource)) = (context, process_resource.as_ref()) {
        context.resources.complete(&resource.id, &status).await?;
    }
    Ok(StepOutput {
        status,
        exit_code,
        stdout: stdout_task
            .await
            .map_err(|error| DevelopmentError::Internal(error.to_string()))??,
        stderr: stderr_task
            .await
            .map_err(|error| DevelopmentError::Internal(error.to_string()))??,
    })
}

fn host_command(command: &str, workspace: &Path, environment: &BTreeMap<String, String>) -> ExecutionCommandSpec {
    #[cfg(unix)]
    let (program, args) = ("sh", vec!["-lc".into(), command.into()]);
    #[cfg(windows)]
    let (program, args) = ("cmd", vec!["/C".into(), command.into()]);
    ExecutionCommandSpec {
        program: program.into(),
        args,
        working_directory: workspace.into(),
        environment: environment.clone(),
    }
}

fn selected_secret_environment(
    input: &CommandExecutionInput<'_>,
) -> Result<(BTreeMap<String, String>, Vec<String>), DevelopmentError> {
    let policy_keys: BTreeSet<String> = serde_json::from_str(&input.policy.allowed_secret_keys_json)
        .map_err(|error| DevelopmentError::BadRequest(format!("invalid policy Secret keys: {error}")))?;
    let runtime_keys: BTreeSet<String> = match input.runtime_profile {
        Some(profile) => serde_json::from_str(&profile.env_keys)
            .map_err(|error| DevelopmentError::BadRequest(format!("invalid runtime env keys: {error}")))?,
        None => policy_keys.clone(),
    };
    let selected = policy_keys
        .intersection(&runtime_keys)
        .filter_map(|key| input.environment.get(key).map(|value| (key.clone(), value.clone())))
        .collect::<BTreeMap<_, _>>();
    let values = selected.values().filter(|value| !value.is_empty()).cloned().collect();
    Ok((selected, values))
}

fn safe_execution_name(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    result.truncate(48);
    let result = result.trim_matches('-');
    if result.is_empty() {
        "execution".into()
    } else {
        result.into()
    }
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(mut reader: R) -> Result<String, std::io::Error> {
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if collected.len() < MAX_OUTPUT_BYTES {
            let remaining = MAX_OUTPUT_BYTES - collected.len();
            collected.extend_from_slice(&buffer[..count.min(remaining)]);
        }
    }
    Ok(String::from_utf8_lossy(&collected).into_owned())
}

fn append_bounded(target: &mut String, value: &str) {
    if target.len() >= MAX_OUTPUT_BYTES {
        return;
    }
    if !target.is_empty() && !value.is_empty() {
        target.push('\n');
    }
    let remaining = MAX_OUTPUT_BYTES.saturating_sub(target.len());
    target.push_str(&value[..value.floor_char_boundary(remaining.min(value.len()))]);
}
