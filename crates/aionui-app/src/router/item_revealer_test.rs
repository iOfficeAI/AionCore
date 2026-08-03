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
    // A missing target must surface as NotFound (item is gone), not RevealFailed.
    let result = revealer().reveal("/nonexistent/path/xyz-12345").await;
    assert!(
        matches!(result, Err(FileError::NotFound(_))),
        "missing path should map to NotFound, got {result:?}"
    );
}
