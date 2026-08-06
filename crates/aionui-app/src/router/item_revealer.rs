//! Composition adapter: implements the file crate's [`IItemRevealer`] port over
//! the shell service's `show_item_in_folder` capability. Lives here (not in
//! `aionui-file`) so the file domain crate needs no dependency on the shell
//! crate — the two domains are wired together only at the composition layer.

use std::sync::Arc;

use aionui_file::{FileError, IItemRevealer};
use aionui_shell::{ShellError, ShellService};

/// Reveals an absolute path in the OS file manager by delegating to the shell
/// service, mapping shell errors onto the file crate's error taxonomy.
pub struct ShellItemRevealer {
    shell: Arc<ShellService>,
}

impl ShellItemRevealer {
    pub fn new(shell: Arc<ShellService>) -> Self {
        Self { shell }
    }
}

#[async_trait::async_trait]
impl IItemRevealer for ShellItemRevealer {
    async fn reveal(&self, absolute_path: &str) -> Result<(), FileError> {
        self.shell.show_item_in_folder(absolute_path).await.map_err(|err| {
            // Classification only — never the path or the shell's message. The
            // caller addressed this by `{pe_id, relative_path}`, so the absolute
            // path was produced server-side and must not travel back out. Both
            // arms deliberately discard their payload; the cause is logged here
            // instead of being carried in the error.
            match err {
                // The target path is gone → not-found, so the client distinguishes
                // "item no longer exists" from "couldn't open the file manager".
                ShellError::FileNotFound(path) | ShellError::DirectoryNotFound(path) => {
                    tracing::warn!(target: "reveal", path = %path, "reveal target does not exist");
                    FileError::TargetNotFound
                }
                other => {
                    tracing::error!(target: "reveal", error = %other, "reveal command failed");
                    FileError::RevealFailed("shell reveal command failed".to_owned())
                }
            }
        })
    }
}

#[cfg(test)]
#[path = "item_revealer_test.rs"]
mod item_revealer_test;
