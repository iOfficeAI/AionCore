use std::sync::Arc;

use aionui_file::{FileError, ISystemFileOpener};
use aionui_shell::{NoopSystemOpener, ShellService};

use super::ShellSystemFileOpener;

fn opener() -> ShellSystemFileOpener {
    ShellSystemFileOpener::new(Arc::new(ShellService::new(Arc::new(NoopSystemOpener))))
}

#[tokio::test]
async fn open_existing_path_succeeds() {
    // The noop opener does not actually launch an application, so a real,
    // existing path passes validation and the open resolves Ok.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("doc.txt");
    std::fs::write(&file, "x").unwrap();

    let result = opener().open(file.to_str().unwrap()).await;
    assert!(
        result.is_ok(),
        "open of an existing path should succeed, got {result:?}"
    );
}

#[tokio::test]
async fn open_missing_path_maps_to_target_not_found() {
    // A missing target must surface as not-found, distinct from "the opener
    // failed", so the client can tell the two apart.
    let result = opener().open("/nonexistent/path/xyz-12345").await;
    assert!(
        matches!(result, Err(FileError::TargetNotFound)),
        "missing path should map to TargetNotFound, got {result:?}"
    );
}

/// INV-OPEN, at the adapter seam: the resolved absolute path must not appear in
/// the error by any rendering. This is the regression guard for the leak the
/// reveal path had — there, the shell error's path was forwarded verbatim.
#[tokio::test]
async fn open_error_never_carries_the_absolute_path() {
    let missing = "/nonexistent/secret-dir/private-file-xyz.txt";
    let err = opener().open(missing).await.expect_err("must fail");

    let rendered = format!("{err}");
    let debug = format!("{err:?}");
    for haystack in [&rendered, &debug] {
        assert!(
            !haystack.contains("secret-dir") && !haystack.contains("private-file-xyz"),
            "error must not disclose the resolved path, got {haystack:?}"
        );
    }
}
