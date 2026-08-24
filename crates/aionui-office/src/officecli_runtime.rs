use std::ffi::{OsStr, OsString};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use aionui_runtime::resolve_command_path;

use crate::error::OfficeError;

pub(crate) const OFFICECLI_INSTALL_SH_URL: &str = "https://raw.githubusercontent.com/suoak/OfficeCLI/main/install.sh";
pub(crate) const OFFICECLI_INSTALL_PS1_URL: &str = "https://raw.githubusercontent.com/suoak/OfficeCLI/main/install.ps1";
pub(crate) const OFFICECLI_LATEST_RELEASE_URL: &str = "https://github.com/suoak/OfficeCLI/releases/latest";
pub(crate) const OFFICECLI_MODE_ENV: &str = "CSBU_WORKMATE_OFFICECLI_MODE";
pub(crate) const OFFICECLI_PATH_ENV: &str = "CSBU_WORKMATE_OFFICECLI_PATH";
const BUNDLED_MODE: &str = "bundled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfficecliInstallPlatform {
    Unix,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficecliInstallCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

pub(crate) fn is_bundled_officecli_mode() -> bool {
    std::env::var_os(OFFICECLI_MODE_ENV).is_some_and(|mode| mode == OsStr::new(BUNDLED_MODE))
}

pub(crate) fn resolve_officecli_path() -> Result<PathBuf, OfficeError> {
    if let Some(result) = resolve_bundled_officecli_path(
        std::env::var_os(OFFICECLI_MODE_ENV).as_deref(),
        std::env::var_os(OFFICECLI_PATH_ENV).as_deref(),
    ) {
        return result;
    }

    resolve_command_path("officecli")
        .or_else(resolve_known_officecli_install_path)
        .ok_or(OfficeError::OfficecliNotFound)
}

fn resolve_bundled_officecli_path(
    mode: Option<&OsStr>,
    configured_path: Option<&OsStr>,
) -> Option<Result<PathBuf, OfficeError>> {
    if mode != Some(OsStr::new(BUNDLED_MODE)) {
        return None;
    }

    Some(
        configured_path
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && is_executable_file(path))
            .ok_or(OfficeError::BundledOfficecliUnavailable),
    )
}

fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn officecli_unavailable_error() -> OfficeError {
    if is_bundled_officecli_mode() {
        OfficeError::BundledOfficecliUnavailable
    } else {
        OfficeError::OfficecliNotFound
    }
}

pub(crate) fn install_command() -> OfficecliInstallCommand {
    if cfg!(windows) {
        install_command_for_platform(OfficecliInstallPlatform::Windows)
    } else {
        install_command_for_platform(OfficecliInstallPlatform::Unix)
    }
}

pub(crate) fn install_command_for_platform(platform: OfficecliInstallPlatform) -> OfficecliInstallCommand {
    match platform {
        OfficecliInstallPlatform::Windows => OfficecliInstallCommand {
            program: OsString::from("powershell.exe"),
            args: vec![
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-Command"),
                OsString::from(format!(
                    "$ErrorActionPreference='Stop'; irm {OFFICECLI_INSTALL_PS1_URL} | iex"
                )),
            ],
        },
        OfficecliInstallPlatform::Unix => OfficecliInstallCommand {
            program: OsString::from("bash"),
            args: vec![
                OsString::from("-lc"),
                OsString::from(format!("curl -fsSL {OFFICECLI_INSTALL_SH_URL} | bash")),
            ],
        },
    }
}

fn resolve_known_officecli_install_path() -> Option<PathBuf> {
    resolve_known_officecli_install_path_from_env(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
    )
}

fn resolve_known_officecli_install_path_from_env(
    home: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(local_app_data) = local_app_data {
        candidates.push(PathBuf::from(local_app_data).join("OfficeCli").join("officecli.exe"));
        candidates.push(
            PathBuf::from(local_app_data)
                .join("Programs")
                .join("officecli")
                .join("officecli.exe"),
        );
    }

    if let Some(home) = home {
        candidates.push(PathBuf::from(home).join(".local").join("bin").join("officecli"));
    }

    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
pub(crate) fn resolve_officecli_path_from_env_for_test(
    path_env: Option<&OsStr>,
    home: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Option<PathBuf> {
    find_officecli_in_path(path_env).or_else(|| {
        resolve_known_officecli_install_path_from_env(home.map(Path::as_os_str), local_app_data.map(Path::as_os_str))
    })
}

#[cfg(test)]
fn find_officecli_in_path(path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join("officecli");
        if candidate.is_file() {
            return Some(candidate);
        }

        #[cfg(windows)]
        {
            let candidate = dir.join("officecli.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn install_command_for_test(platform: OfficecliInstallPlatform) -> OfficecliInstallCommand {
    install_command_for_platform(platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_marker_file(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
    }

    #[test]
    fn officecli_resolution_uses_path_binary_not_legacy_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let path_bin = tmp.path().join("path-bin").join("officecli");
        let legacy_bin = ["runtime", "node", "tools", "officecli", "bin", "officecli"]
            .into_iter()
            .fold(tmp.path().to_path_buf(), |path, segment| path.join(segment));
        write_marker_file(&path_bin);
        write_marker_file(&legacy_bin);

        let path_env = std::env::join_paths([path_bin.parent().unwrap()]).unwrap();
        let resolved = resolve_officecli_path_from_env_for_test(Some(&path_env), Some(tmp.path()), None);

        assert_eq!(resolved, Some(path_bin));
    }

    #[test]
    fn officecli_resolution_discovers_windows_installer_location() {
        let tmp = tempfile::tempdir().unwrap();
        let local_app_data = tmp.path().join("LocalAppData");
        let officecli_exe = local_app_data.join("OfficeCli").join("officecli.exe");
        std::fs::create_dir_all(officecli_exe.parent().unwrap()).unwrap();
        std::fs::write(&officecli_exe, b"fake exe").unwrap();

        let resolved = resolve_officecli_path_from_env_for_test(None, None, Some(&local_app_data));

        assert_eq!(resolved, Some(officecli_exe));
    }

    #[test]
    fn officecli_resolution_discovers_legacy_windows_programs_location() {
        let tmp = tempfile::tempdir().unwrap();
        let local_app_data = tmp.path().join("LocalAppData");
        let officecli_exe = local_app_data.join("Programs").join("officecli").join("officecli.exe");
        std::fs::create_dir_all(officecli_exe.parent().unwrap()).unwrap();
        std::fs::write(&officecli_exe, b"fake exe").unwrap();

        let resolved = resolve_officecli_path_from_env_for_test(None, None, Some(&local_app_data));

        assert_eq!(resolved, Some(officecli_exe));
    }

    #[test]
    fn bundled_resolution_accepts_only_the_configured_absolute_file() {
        let tmp = tempfile::tempdir().unwrap();
        let officecli = tmp.path().join("officecli");
        write_marker_file(&officecli);

        let resolved = resolve_bundled_officecli_path(Some(OsStr::new("bundled")), Some(officecli.as_os_str()));

        assert!(matches!(resolved, Some(Ok(path)) if path == officecli));
    }

    #[test]
    fn bundled_resolution_fails_closed_for_missing_or_relative_paths() {
        let missing = resolve_bundled_officecli_path(Some(OsStr::new("bundled")), None);
        let relative = resolve_bundled_officecli_path(Some(OsStr::new("bundled")), Some(OsStr::new("officecli")));

        assert!(matches!(missing, Some(Err(OfficeError::BundledOfficecliUnavailable))));
        assert!(matches!(relative, Some(Err(OfficeError::BundledOfficecliUnavailable))));
    }

    #[test]
    fn unmanaged_resolution_does_not_consume_the_bundled_path() {
        let resolved = resolve_bundled_officecli_path(None, Some(OsStr::new("/tmp/officecli")));

        assert!(resolved.is_none());
    }

    #[test]
    fn installer_commands_use_branded_officecli_fork() {
        let unix = install_command_for_test(OfficecliInstallPlatform::Unix);
        let windows = install_command_for_test(OfficecliInstallPlatform::Windows);
        let unix_text = format!("{:?} {:?}", unix.program, unix.args);
        let windows_text = format!("{:?} {:?}", windows.program, windows.args);

        assert!(unix_text.contains("suoak/OfficeCLI/main/install.sh"));
        assert!(windows_text.contains("suoak/OfficeCLI/main/install.ps1"));
        assert!(!unix_text.contains("d.officecli.ai"));
        assert!(!windows_text.contains("d.officecli.ai"));
    }
}
