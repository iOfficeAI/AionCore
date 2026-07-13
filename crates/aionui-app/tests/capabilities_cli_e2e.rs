//! E2E coverage for the top-level agent-facing `aioncore capabilities` index.

use tokio::process::Command;

fn aioncore_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aioncore"))
}

#[tokio::test]
async fn top_level_capabilities_prints_domain_index_without_runtime_env() {
    let output = aioncore_command()
        .arg("capabilities")
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_HELPER_BIN")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "capabilities should not need runtime env, stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["meta"]["schema_version"], 1);
    assert_eq!(stdout["data"]["contract"], "agent-facing-aioncore-cli");
    assert_eq!(stdout["data"]["entrypoint"], "aioncore capabilities");
    assert_eq!(stdout["data"]["runtime_context"]["primary"], "AIONUI_CONVERSATION_ID");

    let domains = stdout["data"]["domains"]
        .as_array()
        .expect("domains should be an array");
    let config = domains
        .iter()
        .find(|domain| domain["name"] == "config")
        .expect("config domain should be advertised");
    assert_eq!(config["mode"], "read-write");
    assert_eq!(config["contract_command"], "config capabilities");
    assert_eq!(config["invocation"], "aioncore config capabilities");

    let diagnose = domains
        .iter()
        .find(|domain| domain["name"] == "diagnose")
        .expect("diagnose domain should be advertised");
    assert_eq!(diagnose["mode"], "read-only");
    assert_eq!(diagnose["contract_command"], "diagnose capabilities");
    assert_eq!(diagnose["invocation"], "aioncore diagnose capabilities");
}
