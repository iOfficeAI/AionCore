//! Composition adapter: implements the file crate's [`ISystemFileOpener`] port
//! over the shell service's `open_file` capability. Lives here (not in
//! `aionui-file`) so the file domain crate needs no dependency on the shell
//! crate — the two domains are wired together only at the composition layer.
//!
//! Sibling of [`super::item_revealer`], which reveals the enclosing folder; this
//! one opens the file itself with the OS default application.

use std::sync::Arc;

use aionui_file::{FileError, ISystemFileOpener};
use aionui_shell::{ShellError, ShellService};

/// Opens an absolute path with the OS default application by delegating to the
/// shell service, mapping shell errors onto the file crate's error taxonomy.
pub struct ShellSystemFileOpener {
    shell: Arc<ShellService>,
}

impl ShellSystemFileOpener {
    pub fn new(shell: Arc<ShellService>) -> Self {
        Self { shell }
    }
}

#[async_trait::async_trait]
impl ISystemFileOpener for ShellSystemFileOpener {
    async fn open(&self, absolute_path: &str) -> Result<(), FileError> {
        self.shell.open_file(absolute_path).await.map_err(|err| {
            // INV-OPEN: classification only. The path was resolved server-side from
            // an identity the client sent, so neither it nor the shell's own message
            // may travel back — the cause is logged here instead. Both arms discard
            // their payload on purpose; do not "helpfully" thread it through, which
            // is exactly how the reveal path used to leak absolute paths into
            // response bodies.
            match err {
                ShellError::FileNotFound(path) | ShellError::DirectoryNotFound(path) => {
                    tracing::warn!(target: "open_system", path = %path, "open target does not exist");
                    FileError::TargetNotFound
                }
                other => {
                    tracing::error!(target: "open_system", error = %other, "system open command failed");
                    FileError::RevealFailed("system open command failed".to_owned())
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "system_file_opener_test.rs"]
mod system_file_opener_test;
