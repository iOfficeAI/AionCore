use std::sync::Arc;

use aionui_file::{FileError, IItemRevealer};
use aionui_shell::{NoopSystemOpener, ShellService};

use super::ShellItemRevealer;

fn revealer() -> ShellItemRevealer {
    ShellItemRevealer::new(Arc::new(ShellService::new(Arc::new(NoopSystemOpener))))
}

#[tokio::test]
async fn reveal_existing_path_succeeds() {
    // The noop opener does not actually launch a file manager, so a real,
    // existing path passes validation and the reveal resolves Ok.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("doc.txt");
    std::fs::write(&file, "x").unwrap();

    let result = revealer().reveal(file.to_str().unwrap()).await;
    assert!(
        result.is_ok(),
        "reveal of an existing path should succeed, got {result:?}"
    );
}

#[tokio::test]
async fn reveal_missing_path_maps_to_not_found() {
    // A missing target must surface as not-found (item is gone), not RevealFailed.
    let missing = "/nonexistent/path/xyz-12345";
    let result = revealer().reveal(missing).await;
    assert!(
        matches!(result, Err(FileError::TargetNotFound)),
        "missing path should map to TargetNotFound, got {result:?}"
    );
}

/// The leak this adapter exists to prevent: the caller addressed the target by
/// `{pe_id, relative_path}`, so the absolute path is server-side knowledge and
/// must not ride back out on the error. `TargetNotFound` has no payload, so the
/// guard here is that no error rendering can reproduce the path.
#[tokio::test]
async fn reveal_error_never_carries_the_absolute_path() {
    let missing = "/nonexistent/secret-dir/private-file-xyz.txt";
    let err = revealer().reveal(missing).await.expect_err("must fail");

    let rendered = format!("{err}");
    let debug = format!("{err:?}");
    for haystack in [&rendered, &debug] {
        assert!(
            !haystack.contains("secret-dir") && !haystack.contains("private-file-xyz"),
            "error must not disclose the resolved path, got {haystack:?}"
        );
    }
}
