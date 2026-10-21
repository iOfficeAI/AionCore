use std::fs;
use std::path::Path;

/// Write an executable hook in the host platform's script format.
/// Hook paths are executed by the runtime builder, which supports .cmd on Windows.
pub fn write_script(dir: &Path, name: &str, unix_body: &str, windows_body: &str) -> String {
    let (extension, content) = if cfg!(windows) {
        (
            "cmd",
            format!("@echo off\r\n{}\r\n", windows_body.replace('\n', "\r\n")),
        )
    } else {
        ("sh", format!("#!/bin/sh\n{unix_body}\n"))
    };
    let relative_path = format!("{name}.{extension}");
    let full = dir.join(&relative_path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&full, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
    }
    relative_path
}
