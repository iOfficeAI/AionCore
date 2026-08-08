//! Data-dir endpoint advertisement for port-free local discovery.
//!
//! The running aioncore writes a small JSON file under its data directory.
//! Provision CLI callers resolve that file via `--data-dir` (or the same
//! default the backend uses) — never via caller-provided port or port scan.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use aionui_api_types::{LocalProvisionEndpoint, PROVISION_PROTOCOL_VERSION, PROVISION_SCHEMA_VERSION, ProvisionScope};

use crate::config::AppConfig;

/// Relative path of the endpoint advertisement under a data-dir.
pub const ENDPOINT_RELATIVE_PATH: &str = "runtime/local-provision-endpoint.json";

/// Absolute path of the endpoint advertisement file for `data_dir`.
pub fn endpoint_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ENDPOINT_RELATIVE_PATH)
}

/// Stable installation id derived from the canonical data-dir path.
///
/// Callers cannot select installation/profile as authority; this fingerprint
/// is computed by the backend and re-derived by the CLI for the same path.
pub fn installation_id_for_data_dir(data_dir: &Path) -> String {
    let canonical = fs::canonicalize(data_dir)
        .unwrap_or_else(|_| data_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    let mut hasher = Sha256::new();
    hasher.update(b"aioncore-local-provision-installation-v1:");
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    // 16 hex chars is enough for local uniqueness without leaking full path.
    format!("inst_{}", hex_encode(&digest[..8]))
}

/// Profile id for the data-dir. Currently 1:1 with installation; kept separate
/// so multi-profile installs can evolve without a protocol break.
pub fn profile_id_for_data_dir(data_dir: &Path) -> String {
    format!(
        "prof_{}",
        installation_id_for_data_dir(data_dir).trim_start_matches("inst_")
    )
}

/// Build and write the endpoint advertisement for a running server.
pub fn write_endpoint_for_config(config: &AppConfig, pid: u32, started_at_ms: i64) -> std::io::Result<PathBuf> {
    let host = match config.host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1".to_owned(),
        other => other.to_owned(),
    };
    let endpoint = LocalProvisionEndpoint {
        schema_version: PROVISION_SCHEMA_VERSION,
        protocol_version: PROVISION_PROTOCOL_VERSION,
        installation_id: installation_id_for_data_dir(&config.data_dir),
        profile_id: profile_id_for_data_dir(&config.data_dir),
        pid,
        host: host.clone(),
        port: config.port,
        base_url: format!("http://{host}:{}", config.port),
        identity_mode: config.effective_identity_mode().auth_label().to_owned(),
        aioncore_version: env!("CARGO_PKG_VERSION").to_owned(),
        aionui_version: Some(config.app_version.clone()),
        started_at_ms,
        capabilities: ProvisionScope::ALL.to_vec(),
    };
    write_endpoint(&config.data_dir, &endpoint)
}

pub fn write_endpoint(data_dir: &Path, endpoint: &LocalProvisionEndpoint) -> std::io::Result<PathBuf> {
    let path = endpoint_file_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(endpoint).map_err(json_io_error)?;
    // Atomic-ish write: temp file then rename.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)?;
    // Best-effort owner-only mode on Unix so the file is not world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

pub fn read_endpoint(data_dir: &Path) -> std::io::Result<Option<LocalProvisionEndpoint>> {
    let path = endpoint_file_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let endpoint: LocalProvisionEndpoint = serde_json::from_str(&raw).map_err(json_io_error)?;
    Ok(Some(endpoint))
}

pub fn remove_endpoint(data_dir: &Path) -> std::io::Result<()> {
    let path = endpoint_file_path(data_dir);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn json_io_error(err: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::PROVISION_PROTOCOL_VERSION;

    #[test]
    fn installation_id_is_stable_for_same_path() {
        let dir = std::env::temp_dir().join(format!("aion-prov-inst-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let a = installation_id_for_data_dir(&dir);
        let b = installation_id_for_data_dir(&dir);
        assert_eq!(a, b);
        assert!(a.starts_with("inst_"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_roundtrip_under_data_dir() {
        let dir = std::env::temp_dir().join(format!("aion-prov-ep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let endpoint = LocalProvisionEndpoint {
            schema_version: 1,
            protocol_version: PROVISION_PROTOCOL_VERSION,
            installation_id: installation_id_for_data_dir(&dir),
            profile_id: profile_id_for_data_dir(&dir),
            pid: 42,
            host: "127.0.0.1".into(),
            port: 25808,
            base_url: "http://127.0.0.1:25808".into(),
            identity_mode: "aionpro".into(),
            aioncore_version: "0.1.62".into(),
            aionui_version: Some("2.1.52".into()),
            started_at_ms: 1,
            capabilities: ProvisionScope::ALL.to_vec(),
        };
        write_endpoint(&dir, &endpoint).unwrap();
        let loaded = read_endpoint(&dir).unwrap().expect("endpoint should exist");
        assert_eq!(loaded.port, 25808);
        assert_eq!(loaded.installation_id, endpoint.installation_id);
        assert!(!loaded.base_url.is_empty());

        remove_endpoint(&dir).unwrap();
        assert!(read_endpoint(&dir).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_endpoint_for_config_uses_loopback_for_wildcard_host() {
        let dir = std::env::temp_dir().join(format!("aion-prov-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let config = AppConfig {
            host: "0.0.0.0".into(),
            port: 3001,
            data_dir: dir.clone(),
            work_dir: dir.clone(),
            app_version: "2.0.0".into(),
            local: false,
            identity_mode: crate::config::IdentityMode::AionPro,
            bootstrap_secret: None,
            dump_prompts: false,
            recover_corrupted_database: false,
        };
        write_endpoint_for_config(&config, 7, 99).unwrap();
        let loaded = read_endpoint(&dir).unwrap().unwrap();
        assert_eq!(loaded.host, "127.0.0.1");
        assert_eq!(loaded.base_url, "http://127.0.0.1:3001");
        assert_eq!(loaded.identity_mode, "aionpro");
        let _ = fs::remove_dir_all(&dir);
    }
}
