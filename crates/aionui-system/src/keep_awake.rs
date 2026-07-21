use std::sync::Arc;

use async_trait::async_trait;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
use tokio::sync::Mutex;
use tracing::info;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Stdio;

use crate::error::SystemError;

pub const KEEP_AWAKE_KEY: &str = "keepAwake";

#[async_trait]
pub trait KeepAwakeController: Send + Sync {
    async fn set_enabled(&self, enabled: bool) -> Result<(), SystemError>;
}

pub type DynKeepAwakeController = Arc<dyn KeepAwakeController>;

#[derive(Debug, Default)]
pub struct NoopKeepAwakeController;

#[async_trait]
impl KeepAwakeController for NoopKeepAwakeController {
    async fn set_enabled(&self, _enabled: bool) -> Result<(), SystemError> {
        Ok(())
    }
}

#[derive(Default)]
pub struct SystemKeepAwakeController {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    child: Mutex<Option<tokio::process::Child>>,
    #[cfg(windows)]
    enabled: Mutex<bool>,
}

impl SystemKeepAwakeController {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl KeepAwakeController for SystemKeepAwakeController {
    async fn set_enabled(&self, enabled: bool) -> Result<(), SystemError> {
        set_system_keep_awake(self, enabled).await
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
async fn set_system_keep_awake(controller: &SystemKeepAwakeController, enabled: bool) -> Result<(), SystemError> {
    let mut child = controller.child.lock().await;
    if !enabled {
        if let Some(mut existing) = child.take() {
            let _ = existing.kill().await;
            let _ = existing.wait().await;
            info!("System keep-awake assertion released");
        }
        return Ok(());
    }

    if child.is_some() {
        return Ok(());
    }

    let spawned = spawn_keep_awake_process()?;
    *child = Some(spawned);
    info!("System keep-awake assertion acquired");
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_keep_awake_process() -> Result<tokio::process::Child, SystemError> {
    let mut command = aionui_runtime::Builder::clean_cli("caffeinate");
    command.args(["-dis"]).stdout(Stdio::null()).stderr(Stdio::null());
    command
        .spawn()
        .map_err(|e| SystemError::Internal(format!("Failed to start macOS keep-awake assertion: {e}")))
}

#[cfg(target_os = "linux")]
fn spawn_keep_awake_process() -> Result<tokio::process::Child, SystemError> {
    let mut command = aionui_runtime::Builder::clean_cli("systemd-inhibit");
    command
        .args([
            "--what=sleep",
            "--why=AionUi scheduled tasks keep-awake is enabled",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .map_err(|e| SystemError::Internal(format!("Failed to start Linux keep-awake assertion: {e}")))
}

#[cfg(windows)]
async fn set_system_keep_awake(controller: &SystemKeepAwakeController, enabled: bool) -> Result<(), SystemError> {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    let mut current = controller.enabled.lock().await;
    if *current == enabled {
        return Ok(());
    }

    let flags = if enabled {
        ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
    } else {
        ES_CONTINUOUS
    };
    let result = unsafe { SetThreadExecutionState(flags) };
    if result == 0 {
        return Err(SystemError::Internal(
            "Failed to update Windows keep-awake assertion".into(),
        ));
    }

    *current = enabled;
    if enabled {
        info!("System keep-awake assertion acquired");
    } else {
        info!("System keep-awake assertion released");
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
async fn set_system_keep_awake(_controller: &SystemKeepAwakeController, enabled: bool) -> Result<(), SystemError> {
    if enabled {
        return Err(SystemError::Internal(
            "System keep-awake is not supported on this platform".into(),
        ));
    }
    Ok(())
}
