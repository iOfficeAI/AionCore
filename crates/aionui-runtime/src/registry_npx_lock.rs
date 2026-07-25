use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

const LOCK_JSON: &str = include_str!("../resources/acp-registry-npx-lock.json");

#[derive(Debug, Deserialize)]
struct RegistryNpxLock {
    schema_version: u32,
    agents: BTreeMap<String, RegistryNpxPackage>,
}

#[derive(Debug, Deserialize)]
struct RegistryNpxPackage {
    package: String,
    version: String,
    #[serde(default)]
    skip_version_probe: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryNpxLockError {
    #[error("invalid embedded ACP Registry npx lock: {0}")]
    InvalidLock(String),
    #[error("builtin npx agent '{backend}' has no release-pinned package")]
    MissingBackend { backend: String },
    #[error("builtin npx agent '{backend}' arguments do not contain locked package '{package}'")]
    PackageMismatch { backend: String, package: String },
}

fn registry_npx_lock() -> Result<&'static RegistryNpxLock, RegistryNpxLockError> {
    static LOCK: OnceLock<Result<RegistryNpxLock, String>> = OnceLock::new();
    LOCK.get_or_init(|| {
        let lock: RegistryNpxLock = serde_json::from_str(LOCK_JSON).map_err(|error| error.to_string())?;
        if lock.schema_version != 1 {
            return Err(format!("unsupported schema_version {}", lock.schema_version));
        }
        for (backend, package) in &lock.agents {
            if backend.trim().is_empty() || package.package.trim().is_empty() {
                return Err("backend and package must not be empty".to_owned());
            }
            semver::Version::parse(&package.version)
                .map_err(|error| format!("invalid version for backend '{backend}': {error}"))?;
        }
        Ok(lock)
    })
    .as_ref()
    .map_err(|message| RegistryNpxLockError::InvalidLock(message.clone()))
}

/// Replace the stable npm package identity in a builtin Registry agent's npx
/// arguments with the exact version validated for this AionCore release.
pub fn pin_registry_npx_args(backend: &str, args: &[String]) -> Result<Vec<String>, RegistryNpxLockError> {
    let lock = registry_npx_lock()?;
    let package = lock
        .agents
        .get(backend)
        .ok_or_else(|| RegistryNpxLockError::MissingBackend {
            backend: backend.to_owned(),
        })?;
    let pinned = format!("{}@{}", package.package, package.version);
    let mut found = false;
    let resolved = args
        .iter()
        .map(|arg| {
            if arg == &package.package {
                found = true;
                pinned.clone()
            } else {
                arg.clone()
            }
        })
        .collect();
    if !found {
        return Err(RegistryNpxLockError::PackageMismatch {
            backend: backend.to_owned(),
            package: package.package.clone(),
        });
    }
    Ok(resolved)
}

/// Whether this release has verified that the primary CLI does not implement
/// a bounded `--version` command. PATH presence remains mandatory and the ACP
/// handshake is still required by explicit connection checks.
pub fn should_skip_registry_npx_version_probe(backend: &str) -> Result<bool, RegistryNpxLockError> {
    let lock = registry_npx_lock()?;
    lock.agents
        .get(backend)
        .map(|package| package.skip_version_probe)
        .ok_or_else(|| RegistryNpxLockError::MissingBackend {
            backend: backend.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn pins_direct_npx_package() {
        let args = pin_registry_npx_args("pi", &strings(&["-y", "pi-acp"])).unwrap();
        assert_eq!(args, ["-y", "pi-acp@0.0.31"]);
    }

    #[test]
    fn pins_package_selected_with_package_flag() {
        let args = pin_registry_npx_args(
            "codebuddy",
            &strings(&["-y", "--package", "@tencent-ai/codebuddy-code", "codebuddy", "--acp"]),
        )
        .unwrap();
        assert_eq!(
            args,
            [
                "-y",
                "--package",
                "@tencent-ai/codebuddy-code@2.106.7",
                "codebuddy",
                "--acp"
            ]
        );
    }

    #[test]
    fn every_lock_entry_has_an_exact_version() {
        let lock = registry_npx_lock().unwrap();
        assert_eq!(lock.agents.len(), 11);
        for package in lock.agents.values() {
            assert!(semver::Version::parse(&package.version).is_ok());
        }
    }

    #[test]
    fn rejects_missing_backend_and_package_drift() {
        assert!(matches!(
            pin_registry_npx_args("unknown", &strings(&["-y", "pkg"])),
            Err(RegistryNpxLockError::MissingBackend { .. })
        ));
        assert!(matches!(
            pin_registry_npx_args("pi", &strings(&["-y", "other-package"])),
            Err(RegistryNpxLockError::PackageMismatch { .. })
        ));
    }

    #[test]
    fn only_sigit_skips_the_primary_cli_version_probe() {
        assert!(should_skip_registry_npx_version_probe("sigit").unwrap());
        assert!(!should_skip_registry_npx_version_probe("pi").unwrap());
    }
}
