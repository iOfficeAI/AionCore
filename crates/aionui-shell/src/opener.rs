use aionui_runtime::Builder as CmdBuilder;

use crate::error::ShellError;

#[async_trait::async_trait]
pub trait ISystemOpener: Send + Sync {
    fn open_detached(&self, target: &str) -> Result<(), ShellError>;
    async fn run_command(&self, program: &str, args: &[&str]) -> Result<(), ShellError>;
    fn is_tool_available(&self, tool_name: &str) -> bool;
    /// Write `text` to the OS clipboard. Used by the copy-absolute-path endpoint
    /// so the resolved path is placed on the clipboard entirely server-side and
    /// never returned to the client (mirrors the reveal/open capabilities: the
    /// backend performs the OS action itself). Fails on headless/no-clipboard
    /// environments rather than panicking.
    fn copy_to_clipboard(&self, text: &str) -> Result<(), ShellError>;
}

pub struct DefaultSystemOpener;

#[async_trait::async_trait]
impl ISystemOpener for DefaultSystemOpener {
    fn open_detached(&self, target: &str) -> Result<(), ShellError> {
        open::that_detached(target).map_err(|e| ShellError::CommandFailed(format!("open: {e}")))?;
        Ok(())
    }

    async fn run_command(&self, program: &str, args: &[&str]) -> Result<(), ShellError> {
        let mut builder = CmdBuilder::clean_cli(program);
        builder
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let output = builder
            .spawn()
            .map_err(|e| ShellError::CommandFailed(format!("{program}: {e}")))?;

        let result = output
            .wait_with_output()
            .await
            .map_err(|e| ShellError::CommandFailed(format!("{program}: {e}")))?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!(program, ?args, %stderr, "command exited with non-zero status");
        }
        Ok(())
    }

    fn is_tool_available(&self, tool_name: &str) -> bool {
        which::which(tool_name).is_ok()
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<(), ShellError> {
        // arboard is cross-platform (mac/win/linux). Any failure — including a
        // headless/no-display environment (e.g. a remote WebUI server) — maps to a
        // command error rather than a panic. aioncore is long-lived, so on X11 it
        // remains the clipboard owner and the content persists after this returns.
        let mut clipboard =
            arboard::Clipboard::new().map_err(|e| ShellError::CommandFailed(format!("clipboard: {e}")))?;
        clipboard
            .set_text(text.to_owned())
            .map_err(|e| ShellError::CommandFailed(format!("clipboard: {e}")))
    }
}

pub struct NoopSystemOpener;

#[async_trait::async_trait]
impl ISystemOpener for NoopSystemOpener {
    fn open_detached(&self, _target: &str) -> Result<(), ShellError> {
        Ok(())
    }

    async fn run_command(&self, _program: &str, _args: &[&str]) -> Result<(), ShellError> {
        Ok(())
    }

    fn is_tool_available(&self, _tool_name: &str) -> bool {
        true
    }

    fn copy_to_clipboard(&self, _text: &str) -> Result<(), ShellError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_opener_detects_nonexistent_tool() {
        let opener = DefaultSystemOpener;
        assert!(!opener.is_tool_available("__nonexistent_tool_xyz__"));
    }

    #[test]
    fn noop_opener_open_detached_succeeds() {
        let opener = NoopSystemOpener;
        assert!(opener.open_detached("https://example.com").is_ok());
    }

    #[tokio::test]
    async fn noop_opener_run_command_succeeds() {
        let opener = NoopSystemOpener;
        assert!(opener.run_command("fake-program", &["arg1"]).await.is_ok());
    }

    #[test]
    fn noop_opener_is_tool_available_always_true() {
        let opener = NoopSystemOpener;
        assert!(opener.is_tool_available("__nonexistent__"));
    }
}
