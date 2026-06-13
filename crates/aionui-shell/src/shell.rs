use std::path::Path;
use std::sync::Arc;

use aionui_api_types::ToolType;

use crate::error::ShellError;
use crate::opener::ISystemOpener;

const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "mailto"];

pub struct ShellService {
    opener: Arc<dyn ISystemOpener>,
}

impl ShellService {
    pub fn new(opener: Arc<dyn ISystemOpener>) -> Self {
        Self { opener }
    }

    pub async fn open_file(&self, file_path: &str) -> Result<(), ShellError> {
        let path = validate_file_exists(file_path)?;
        self.opener.open_detached(&path.to_string_lossy())
    }

    pub async fn show_item_in_folder(&self, file_path: &str) -> Result<(), ShellError> {
        let path = validate_path_exists(file_path)?;
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &["-R", &path.to_string_lossy()]).await
        } else if cfg!(target_os = "windows") {
            let parent = path.parent().unwrap_or(&path);
            self.opener.run_command("explorer", &[&parent.to_string_lossy()]).await
        } else {
            // Linux: prefer the freedesktop D-Bus FileManager1.ShowItems method,
            // which opens the file manager *and* highlights the target file. Fall
            // back to opening the parent directory with `xdg-open` when `gdbus` is
            // unavailable. Opening the file/dir directly via `xdg-open` is wrong
            // here: it resolves the path's MIME handler, which on some desktops is
            // a text editor rather than the file manager.
            let gdbus_available = self.opener.is_tool_available("gdbus");
            let (program, args) = linux_show_item_command(&path, gdbus_available);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            self.opener.run_command(&program, &arg_refs).await
        }
    }

    pub async fn open_external(&self, url: &str) -> Result<(), ShellError> {
        validate_url(url)?;
        self.opener.open_detached(url)
    }

    pub async fn check_tool_installed(&self, tool: ToolType) -> bool {
        match tool {
            ToolType::Terminal | ToolType::Explorer => true,
            ToolType::Vscode => self.detect_vscode(),
        }
    }

    pub async fn open_folder_with(&self, folder_path: &str, tool: ToolType) -> Result<(), ShellError> {
        let path = validate_directory_exists(folder_path)?;
        match tool {
            ToolType::Vscode => self.open_folder_vscode(&path).await,
            ToolType::Terminal => self.open_folder_terminal(&path).await,
            ToolType::Explorer => self.open_folder_explorer(&path).await,
        }
    }

    fn detect_vscode(&self) -> bool {
        if self.opener.is_tool_available("code") {
            return true;
        }
        if cfg!(target_os = "macos") {
            let app_path = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code";
            return Path::new(app_path).exists();
        }
        false
    }

    async fn open_folder_vscode(&self, path: &Path) -> Result<(), ShellError> {
        if !self.detect_vscode() {
            return Err(ShellError::ToolNotInstalled("vscode".to_owned()));
        }
        self.opener.run_command("code", &[&path.to_string_lossy()]).await
    }

    async fn open_folder_terminal(&self, path: &Path) -> Result<(), ShellError> {
        let path_str = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &["-a", "Terminal", &path_str]).await
        } else if cfg!(target_os = "windows") {
            let command = build_windows_terminal_command(&path_str);
            self.opener
                .run_command("cmd", &["/c", "start", "cmd", "/K", &command])
                .await
        } else {
            self.try_linux_terminal(&path_str).await
        }
    }

    async fn open_folder_explorer(&self, path: &Path) -> Result<(), ShellError> {
        let path_str = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &[&path_str]).await
        } else if cfg!(target_os = "windows") {
            self.opener.run_command("explorer", &[&path_str]).await
        } else {
            self.opener.run_command("xdg-open", &[&path_str]).await
        }
    }

    async fn try_linux_terminal(&self, path: &str) -> Result<(), ShellError> {
        let terminals = [
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "x-terminal-emulator",
            "terminator",
        ];
        for term in &terminals {
            if self.opener.is_tool_available(term) {
                let args: Vec<&str> = match *term {
                    "gnome-terminal" => vec!["--working-directory", path],
                    "konsole" => vec!["--workdir", path],
                    _ => vec!["--working-directory", path],
                };
                return self.opener.run_command(term, &args).await;
            }
        }
        Err(ShellError::ToolNotInstalled("terminal emulator".to_owned()))
    }
}

fn validate_file_exists(file_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(file_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::FileNotFound(file_path.to_owned()))?;
    if !canonical.is_file() {
        return Err(ShellError::FileNotFound(file_path.to_owned()));
    }
    Ok(canonical)
}

fn validate_path_exists(file_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(file_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::FileNotFound(file_path.to_owned()))?;
    if !canonical.exists() {
        return Err(ShellError::FileNotFound(file_path.to_owned()));
    }
    Ok(canonical)
}

fn validate_directory_exists(dir_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(dir_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::DirectoryNotFound(dir_path.to_owned()))?;
    if !canonical.is_dir() {
        return Err(ShellError::DirectoryNotFound(dir_path.to_owned()));
    }
    Ok(canonical)
}

fn build_windows_terminal_command(path: &str) -> String {
    format!(r#"pushd "{path}""#)
}

/// Build the `(program, args)` used to reveal `path` in the Linux file manager.
///
/// When `gdbus` is available we call `org.freedesktop.FileManager1.ShowItems`,
/// which opens the file manager and highlights the file — mirroring Electron's
/// native `shell.showItemInFolder` behavior. The parameters are passed as raw
/// argv items (no shell), so GVariant literals like `['file:///x']` and the
/// empty startup-id `''` reach `gdbus` verbatim.
///
/// Without `gdbus`, fall back to opening the parent directory with `xdg-open`
/// (no highlight, but still the file manager rather than the file's handler).
fn linux_show_item_command(path: &Path, gdbus_available: bool) -> (String, Vec<String>) {
    if gdbus_available {
        // `from_file_path` percent-encodes spaces and other reserved characters;
        // fall back to a raw `file://` URI only if it rejects the path (it
        // requires an absolute path — always true here since `path` is canonical).
        let uri = reqwest::Url::from_file_path(path)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| format!("file://{}", path.to_string_lossy()));
        (
            "gdbus".to_owned(),
            vec![
                "call".to_owned(),
                "--session".to_owned(),
                "--dest".to_owned(),
                "org.freedesktop.FileManager1".to_owned(),
                "--object-path".to_owned(),
                "/org/freedesktop/FileManager1".to_owned(),
                "--method".to_owned(),
                "org.freedesktop.FileManager1.ShowItems".to_owned(),
                format!("['{uri}']"),
                String::new(),
            ],
        )
    } else {
        let parent = path.parent().unwrap_or(path);
        ("xdg-open".to_owned(), vec![parent.to_string_lossy().into_owned()])
    }
}

fn validate_url(url: &str) -> Result<(), ShellError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ShellError::InvalidUrl(url.to_owned()))?;
    if !ALLOWED_URL_SCHEMES.contains(&parsed.scheme()) {
        return Err(ShellError::InvalidUrl(format!(
            "scheme '{}' is not allowed",
            parsed.scheme()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::opener::NoopSystemOpener;
    use std::fs;

    #[test]
    fn validate_file_exists_succeeds_for_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_file_exists(file_path.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_file_exists_fails_for_missing_file() {
        let result = validate_file_exists("/nonexistent/file.txt");
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_file_exists_fails_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_file_exists(dir.path().to_str().unwrap());
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_path_exists_succeeds_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_path_exists(file_path.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_exists_succeeds_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_path_exists(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_exists_fails_for_nonexistent() {
        let result = validate_path_exists("/nonexistent/path");
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_directory_exists_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_directory_exists(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_directory_exists_fails_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_directory_exists(file_path.to_str().unwrap());
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn validate_directory_exists_fails_for_nonexistent() {
        let result = validate_directory_exists("/nonexistent/dir");
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn validate_url_accepts_http() {
        assert!(validate_url("http://example.com").is_ok());
    }

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com/path?q=1").is_ok());
    }

    #[test]
    fn validate_url_accepts_mailto() {
        assert!(validate_url("mailto:user@example.com").is_ok());
    }

    #[test]
    fn validate_url_rejects_file_scheme() {
        let result = validate_url("file:///etc/passwd");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_ftp_scheme() {
        let result = validate_url("ftp://example.com");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_javascript_scheme() {
        let result = validate_url("javascript:alert(1)");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_invalid_url() {
        let result = validate_url("; rm -rf /");
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[test]
    fn validate_url_rejects_empty_string() {
        let result = validate_url("");
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn check_tool_terminal_always_true() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(svc.check_tool_installed(ToolType::Terminal).await);
    }

    #[tokio::test]
    async fn check_tool_explorer_always_true() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(svc.check_tool_installed(ToolType::Explorer).await);
    }

    #[tokio::test]
    async fn open_file_fails_for_missing_file() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_file("/nonexistent/file.txt").await;
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[tokio::test]
    async fn show_item_in_folder_fails_for_missing_path() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.show_item_in_folder("/nonexistent/path").await;
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn linux_show_item_uses_filemanager1_when_gdbus_available() {
        let path = Path::new("/home/user/Downloads/AionUi.deb");
        let (program, args) = linux_show_item_command(path, true);
        assert_eq!(program, "gdbus");
        assert_eq!(args[0], "call");
        assert!(args.iter().any(|a| a == "org.freedesktop.FileManager1"));
        assert!(args.iter().any(|a| a == "org.freedesktop.FileManager1.ShowItems"));
        // The file URI must target the file itself (so it gets highlighted),
        // not the parent directory, wrapped as a GVariant array literal.
        assert!(args.contains(&"['file:///home/user/Downloads/AionUi.deb']".to_owned()));
        // Trailing empty startup-id GVariant string.
        assert_eq!(args.last().unwrap(), "");
    }

    #[test]
    fn linux_show_item_percent_encodes_spaces_in_uri() {
        let path = Path::new("/home/user/My Downloads/AionUi.deb");
        let (program, args) = linux_show_item_command(path, true);
        assert_eq!(program, "gdbus");
        assert!(
            args.contains(&"['file:///home/user/My%20Downloads/AionUi.deb']".to_owned()),
            "space must be percent-encoded, got: {args:?}"
        );
    }

    #[test]
    fn linux_show_item_falls_back_to_parent_dir_without_gdbus() {
        let path = Path::new("/home/user/Downloads/AionUi.deb");
        let (program, args) = linux_show_item_command(path, false);
        assert_eq!(program, "xdg-open");
        // Fallback opens the parent directory, never the file (whose MIME
        // handler may be a text editor) — that was the original bug.
        assert_eq!(args, vec!["/home/user/Downloads".to_owned()]);
    }

    #[tokio::test]
    async fn open_external_fails_for_invalid_url() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_external("; rm -rf /").await;
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn open_external_fails_for_file_scheme() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_external("file:///etc/passwd").await;
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn open_folder_with_fails_for_missing_dir() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_folder_with("/nonexistent/dir", ToolType::Explorer).await;
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[tokio::test]
    async fn open_folder_with_fails_for_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "data").unwrap();
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc
            .open_folder_with(file_path.to_str().unwrap(), ToolType::Explorer)
            .await;
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn build_windows_terminal_command_quotes_paths_with_spaces() {
        let command = build_windows_terminal_command(r#"C:\Users\zhoukai\My Project"#);
        assert_eq!(command, r#"pushd "C:\Users\zhoukai\My Project""#);
    }

    #[test]
    fn build_windows_terminal_command_supports_unc_paths() {
        let command = build_windows_terminal_command(r#"\\server\share\My Project"#);
        assert_eq!(command, r#"pushd "\\server\share\My Project""#);
    }
}
