//! E2E coverage for `aioncore provision` capability advertisement and discovery.

use std::fs;
use std::process::Stdio;

use tokio::process::Command;

fn aioncore_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aioncore"))
}

#[tokio::test]
async fn provision_capabilities_prints_contract_without_runtime_env() {
    let output = aioncore_command()
        .args(["provision", "capabilities"])
        .env_remove("AIONUI_BASE_URL")
        .env_remove("AIONUI_CONVERSATION_ID")
        .env_remove("AIONUI_USER_ID")
        .env_remove("AIONUI_RUNTIME_TOKEN")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "provision capabilities failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["contract"], "trusted-local-provisioning");
    assert_eq!(stdout["data"]["protocol_version"], 1);
    assert_eq!(stdout["data"]["discovery"]["caller_port_required"], false);
    assert_eq!(stdout["data"]["discovery"]["method"], "data_dir_endpoint_file");

    let scopes = stdout["data"]["authorization"]["scopes"].as_array().expect("scopes");
    let names: Vec<&str> = scopes.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(names.contains(&"assistant_management"));
    assert!(names.contains(&"mcp_configuration"));
    assert!(names.contains(&"skill_registration"));
    assert!(names.contains(&"team_definition"));
}

#[tokio::test]
async fn provision_discover_reports_closed_without_endpoint_file() {
    let dir = tempfile_dir("provision-discover-closed");
    let output = aioncore_command()
        .args(["--data-dir"])
        .arg(&dir)
        .args(["provision", "discover"])
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "provision discover failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], true);
    assert_eq!(stdout["data"]["caller_port_required"], false);
    assert_eq!(stdout["data"]["discovery_method"], "data_dir_endpoint_file");
    assert_eq!(stdout["data"]["backend"]["state"], "closed");
    assert!(stdout["data"]["installation_id"].as_str().unwrap().starts_with("inst_"));

    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn provision_authorize_fails_closed_without_attested_subject() {
    let dir = tempfile_dir("provision-authorize-no-subject");

    let discover = aioncore_command()
        .args(["--data-dir"])
        .arg(&dir)
        .args(["provision", "discover"])
        .output()
        .await
        .unwrap();
    assert!(discover.status.success());
    let discover_json: serde_json::Value = serde_json::from_slice(&discover.stdout).unwrap();
    let installation_id = discover_json["data"]["installation_id"].as_str().unwrap();
    let profile_id = discover_json["data"]["profile_id"].as_str().unwrap();

    let payload = serde_json::json!({
        "protocol_version": 1,
        "installation_id": installation_id,
        "profile_id": profile_id,
        "scopes": ["assistant_management"]
    });

    let mut child = aioncore_command()
        .args(["--data-dir"])
        .arg(&dir)
        .args(["provision", "authorize"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(payload.to_string().as_bytes()).await.unwrap();
    drop(stdin);

    let output = child.wait_with_output().await.unwrap();
    assert!(
        !output.status.success(),
        "authorize should fail closed without attested subject\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stdout["success"], false);
    assert_eq!(stdout["error"]["code"], "PROVISION_AUTHORITY_MISSING");
    assert_eq!(stdout["error"]["zero_mutation"], true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("PROVISION_AUTHORITY_MISSING"));

    let _ = fs::remove_dir_all(&dir);
}

fn tempfile_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aioncore-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}
