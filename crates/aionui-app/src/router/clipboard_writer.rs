//! Composition adapter: implements the file crate's [`IClipboardWriter`] port
//! over the shell service's `copy_text_to_clipboard` capability. Lives here (not
//! in `aionui-file`) so the file domain crate needs no dependency on the shell
//! crate — the two domains are wired together only at the composition layer.
//!
//! Sibling of [`super::item_revealer`] / [`super::system_file_opener`]: the
//! backend resolves the path server-side and performs the OS action (here, a
//! clipboard write) itself, so the resolved absolute path is never returned to
//! the client.

use std::sync::Arc;

use aionui_file::{FileError, IClipboardWriter};
use aionui_shell::ShellService;

/// Writes text to the OS clipboard by delegating to the shell service, mapping
/// shell errors onto the file crate's error taxonomy.
pub struct ShellClipboardWriter {
    shell: Arc<ShellService>,
}

impl ShellClipboardWriter {
    pub fn new(shell: Arc<ShellService>) -> Self {
        Self { shell }
    }
}

#[async_trait::async_trait]
impl IClipboardWriter for ShellClipboardWriter {
    async fn write_text(&self, text: &str) -> Result<(), FileError> {
        self.shell.copy_text_to_clipboard(text).await.map_err(|err| {
            // Classification only — the error carries no path or shell message
            // (the text written is the server-resolved absolute path, which must
            // not travel back out). A headless/no-clipboard environment lands here
            // too rather than panicking; the cause is logged, not returned.
            tracing::error!(target: "copy_absolute_path", error = %err, "clipboard write failed");
            FileError::Internal("clipboard write failed".to_owned())
        })
    }
}
