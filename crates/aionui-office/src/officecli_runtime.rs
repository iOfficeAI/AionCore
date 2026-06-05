use std::path::{Path, PathBuf};

use aionui_runtime::{ResolvedCommand, ensure_runtime_command};

use crate::error::OfficeError;

pub(crate) fn officecli_prefix(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime").join("node").join("tools").join("officecli")
}

pub(crate) fn resolve_officecli_path(data_dir: &Path) -> Option<PathBuf> {
    let prefix = officecli_prefix(data_dir);
    let bin = if cfg!(windows) {
        prefix.join("bin").join("officecli.cmd")
    } else {
        prefix.join("bin").join("officecli")
    };
    bin.is_file().then_some(bin)
}

pub(crate) async fn resolve_officecli_command(data_dir: &Path) -> Result<ResolvedCommand, OfficeError> {
    let path = resolve_officecli_path(data_dir).ok_or(OfficeError::OfficecliNotFound)?;
    let runtime = ensure_runtime_command("npm")
        .await
        .map_err(|e| OfficeError::InstallFailed(e.to_string()))?;
    Ok(ResolvedCommand {
        program: path,
        args_prefix: vec![],
        env: runtime.env,
    })
}
