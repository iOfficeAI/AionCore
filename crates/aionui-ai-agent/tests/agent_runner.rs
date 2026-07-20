use std::time::Duration;

use aionui_ai_agent::capability::cli_process::CliAgentProcess;
use aionui_common::{CommandSpec, EnvVar};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.into(),
        value: value.into(),
    }
}

#[tokio::test]
async fn acp_host_process_runs_through_a_named_execution_lease() {
    let data_dir = tempfile::tempdir().unwrap();
    let process = CliAgentProcess::spawn_for_sdk(
        CommandSpec {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s\\n' \"$AIONUI_RUNNER_ENVIRONMENT_ID\"; read line".into(),
            ],
            env: vec![
                env("AIONUI_RUNNER_ENVIRONMENT_KIND", "host"),
                env("AIONUI_RUNNER_ENVIRONMENT_ID", "host:test"),
                env("AIONUI_EXECUTION_LEASE_ID", "lease-agent-test"),
            ],
            cwd: None,
        },
        data_dir.path(),
    )
    .await
    .unwrap();
    let (mut stdin, stdout) = process.take_stdio().await.unwrap();
    let mut lines = BufReader::new(stdout).lines();
    assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("host:test"));
    stdin.write_all(b"stop\n").await.unwrap();
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(2), process.wait_for_exit())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn container_agent_modes_fail_closed_when_the_selected_environment_is_incomplete() {
    let data_dir = tempfile::tempdir().unwrap();
    let docker = match CliAgentProcess::spawn_for_sdk(
        CommandSpec {
            command: "agent".into(),
            args: Vec::new(),
            env: vec![env("AIONUI_RUNNER_ENVIRONMENT_KIND", "docker")],
            cwd: Some(data_dir.path().to_string_lossy().into_owned()),
        },
        data_dir.path(),
    )
    .await
    {
        Ok(process) => {
            process.kill(Duration::from_millis(100)).await.unwrap();
            panic!("incomplete Docker runner configuration unexpectedly started")
        }
        Err(error) => error,
    };
    assert!(docker.to_string().contains("container image"));

    let devcontainer = match CliAgentProcess::spawn_for_sdk(
        CommandSpec {
            command: "agent".into(),
            args: Vec::new(),
            env: vec![env("AIONUI_RUNNER_ENVIRONMENT_KIND", "devcontainer")],
            cwd: Some(data_dir.path().to_string_lossy().into_owned()),
        },
        data_dir.path(),
    )
    .await
    {
        Ok(process) => {
            process.kill(Duration::from_millis(100)).await.unwrap();
            panic!("incomplete Dev Container runner configuration unexpectedly started")
        }
        Err(error) => error,
    };
    assert!(devcontainer.to_string().contains("config path"));

    let devcontainer_with_host_secret = match CliAgentProcess::spawn_for_sdk(
        CommandSpec {
            command: "agent".into(),
            args: Vec::new(),
            env: vec![
                env("AIONUI_RUNNER_ENVIRONMENT_KIND", "devcontainer"),
                env("AIONUI_RUNNER_DEVCONTAINER_CONFIG", ".devcontainer/devcontainer.json"),
                env("AGENT_TOKEN", "must-not-become-a-command-argument"),
            ],
            cwd: Some(data_dir.path().to_string_lossy().into_owned()),
        },
        data_dir.path(),
    )
    .await
    {
        Ok(process) => {
            process.kill(Duration::from_millis(100)).await.unwrap();
            panic!("Dev Container runner unexpectedly forwarded a host secret")
        }
        Err(error) => error,
    };
    assert!(
        devcontainer_with_host_secret
            .to_string()
            .contains("provisioned inside the selected container")
    );
}
