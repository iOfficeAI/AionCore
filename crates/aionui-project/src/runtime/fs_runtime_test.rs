use std::sync::Arc;

use super::{FsRuntimeRegistry, IFsRuntime, IoDispatch, LocalFsRuntime};

#[tokio::test]
async fn local_runtime_is_inline_and_serves_file_scheme() {
    let (runtime, _rx) = LocalFsRuntime::new().unwrap();
    assert_eq!(runtime.io_dispatch(), IoDispatch::Inline);
    assert_eq!(runtime.provider().scheme(), "file");
}

#[tokio::test]
async fn registry_dispatches_by_scheme() {
    let (runtime, _rx) = LocalFsRuntime::new().unwrap();
    let mut registry = FsRuntimeRegistry::new();
    registry.register("file", Arc::new(runtime));

    let got = registry.get("file").expect("file runtime registered");
    assert_eq!(got.provider().scheme(), "file");
    assert!(registry.get("ssh").is_none());
}
