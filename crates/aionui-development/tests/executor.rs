use std::collections::BTreeMap;

use aionui_db::models::{DevelopmentPolicyRow, ProjectRuntimeProfileRow};
use aionui_development::{CommandExecutionInput, build_execution_plan, execute_command};

fn policy(mode: &str) -> DevelopmentPolicyRow {
    DevelopmentPolicyRow {
        id: "policy".into(),
        user_id: "user".into(),
        project_id: "project".into(),
        isolation_mode: mode.into(),
        container_image: (mode == "docker").then(|| "node:24-alpine".into()),
        devcontainer_config_path: (mode == "devcontainer").then(|| ".devcontainer/devcontainer.json".into()),
        container_cpu_millis: 750,
        container_memory_mb: 1024,
        container_pids_limit: 128,
        network_mode: "none".into(),
        allowed_secret_keys_json: "[\"NPM_TOKEN\",\"IGNORED_TOKEN\"]".into(),
        max_duration_ms: 60_000,
        max_parallel_agents: 2,
        max_retries: 1,
        max_cost_microunits: 0,
        alert_percent: 80,
        over_limit_action: "pause".into(),
        created_at: 1,
        updated_at: 1,
    }
}

fn runtime() -> ProjectRuntimeProfileRow {
    ProjectRuntimeProfileRow {
        project_id: "project".into(),
        environment_kind: "node".into(),
        language: Some("typescript".into()),
        package_manager: Some("bun".into()),
        runtime_version: Some("24".into()),
        env_keys: "[\"NPM_TOKEN\",\"PUBLIC_FLAG\"]".into(),
        metadata: "{}".into(),
        updated_at: 1,
    }
}

#[test]
fn host_plan_preserves_compatibility_without_container_arguments() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = build_execution_plan(&CommandExecutionInput {
        execution_id: "gate-host",
        run_id: "run-host",
        command: "bun test",
        working_directory: workspace.path(),
        timeout_seconds: 30,
        policy: &policy("host"),
        runtime_profile: Some(&runtime()),
        environment: BTreeMap::new(),
    })
    .unwrap();
    assert_eq!(plan.isolation_mode, "host");
    assert_eq!(plan.steps.len(), 1);
    assert!(matches!(plan.steps[0].program.as_str(), "sh" | "cmd"));
    assert!(!plan.steps[0].args.iter().any(|arg| arg == "docker"));
}

#[test]
fn docker_plan_is_unprivileged_bounded_and_passes_secrets_by_name() {
    let workspace = tempfile::tempdir().unwrap();
    let environment = BTreeMap::from([
        ("NPM_TOKEN".into(), "do-not-put-this-in-args".into()),
        ("IGNORED_TOKEN".into(), "not-in-runtime-allowlist".into()),
        ("PUBLIC_FLAG".into(), "1".into()),
    ]);
    let plan = build_execution_plan(&CommandExecutionInput {
        execution_id: "gate-docker",
        run_id: "run-docker",
        command: "bun test",
        working_directory: workspace.path(),
        timeout_seconds: 30,
        policy: &policy("docker"),
        runtime_profile: Some(&runtime()),
        environment,
    })
    .unwrap();
    let step = &plan.steps[0];
    assert_eq!(step.program, "docker");
    let joined = step.args.join(" ");
    for required in [
        "run",
        "--rm",
        "--init",
        "--cap-drop ALL",
        "no-new-privileges",
        "--pids-limit 128",
        "--cpus 0.750",
        "--memory 1024m",
        "--network none",
        "--env NPM_TOKEN",
        "--workdir /workspace",
        "node:24-alpine",
    ] {
        assert!(joined.contains(required), "missing Docker safety argument: {required}");
    }
    assert!(!joined.contains("do-not-put-this-in-args"));
    assert!(!joined.contains("IGNORED_TOKEN"));
    assert!(!joined.contains("/var/run/docker.sock"));
    assert_eq!(plan.cleanup.as_ref().unwrap().program, "docker");
}

#[test]
fn devcontainer_plan_uses_project_bound_config_and_never_embeds_secret_values() {
    let workspace = tempfile::tempdir().unwrap();
    let config_dir = workspace.path().join(".devcontainer");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("devcontainer.json"), "{}").unwrap();
    let plan = build_execution_plan(&CommandExecutionInput {
        execution_id: "gate-devcontainer",
        run_id: "run-devcontainer",
        command: "bun test",
        working_directory: workspace.path(),
        timeout_seconds: 30,
        policy: &policy("devcontainer"),
        runtime_profile: Some(&runtime()),
        environment: BTreeMap::from([("NPM_TOKEN".into(), "secret-value".into())]),
    })
    .unwrap();
    assert_eq!(plan.steps.len(), 2);
    assert!(plan.steps.iter().all(|step| step.program == "devcontainer"));
    assert_eq!(plan.steps[0].args[0], "up");
    assert_eq!(plan.steps[1].args[0], "exec");
    assert!(
        !plan
            .steps
            .iter()
            .flat_map(|step| &step.args)
            .any(|arg| arg.contains("secret-value"))
    );

    let mut escaping = policy("devcontainer");
    escaping.devcontainer_config_path = Some("../devcontainer.json".into());
    assert!(
        build_execution_plan(&CommandExecutionInput {
            execution_id: "escape",
            run_id: "run",
            command: "true",
            working_directory: workspace.path(),
            timeout_seconds: 30,
            policy: &escaping,
            runtime_profile: None,
            environment: BTreeMap::new(),
        })
        .is_err()
    );
}

#[tokio::test]
async fn host_execution_redacts_configured_secrets_and_times_out() {
    let workspace = tempfile::tempdir().unwrap();
    let environment = BTreeMap::from([("NPM_TOKEN".into(), "super-secret-value".into())]);
    let passed = execute_command(CommandExecutionInput {
        execution_id: "host-redaction",
        run_id: "run-host",
        command: "printf '%s' \"$NPM_TOKEN\"",
        working_directory: workspace.path(),
        timeout_seconds: 5,
        policy: &policy("host"),
        runtime_profile: Some(&runtime()),
        environment: environment.clone(),
    })
    .await
    .unwrap();
    assert_eq!(passed.status, "passed");
    assert_eq!(passed.stdout, "[REDACTED]");

    let timed_out = execute_command(CommandExecutionInput {
        execution_id: "host-timeout",
        run_id: "run-host",
        command: if cfg!(windows) {
            "ping -n 3 127.0.0.1"
        } else {
            "sleep 2"
        },
        working_directory: workspace.path(),
        timeout_seconds: 1,
        policy: &policy("host"),
        runtime_profile: None,
        environment,
    })
    .await
    .unwrap();
    assert_eq!(timed_out.status, "timed_out");
    assert_eq!(timed_out.exit_code, None);
}
