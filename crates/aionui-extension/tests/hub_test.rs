//! Integration tests for Hub extension marketplace (test-plan HM scenarios).

use std::sync::Arc;

use aionui_extension::hub::{HubIndexManager, HubInstaller};
use aionui_extension::registry::ExtensionRegistry;
use aionui_extension::state::ExtensionStateStore;
use aionui_extension::types::HubExtensionStatus;
use aionui_realtime::BroadcastEventBus;
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

struct TestHarness {
    hub_dir: TempDir,
    index_manager: HubIndexManager,
    installer: HubInstaller,
    registry: ExtensionRegistry,
    _state_dir: TempDir,
}

fn setup() -> TestHarness {
    let hub_dir = TempDir::new().unwrap();
    let state_dir = TempDir::new().unwrap();

    let store = ExtensionStateStore::new(state_dir.path().join("states.json"));
    let bus = Arc::new(BroadcastEventBus::new(64));
    let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());

    let index_manager = HubIndexManager::new(hub_dir.path().to_path_buf(), registry.clone());
    let installer = HubInstaller::new(index_manager.clone(), registry.clone());

    TestHarness {
        hub_dir,
        index_manager,
        installer,
        registry,
        _state_dir: state_dir,
    }
}

fn write_hub_index(hub_dir: &std::path::Path, extensions: &[serde_json::Value]) {
    let index = json!({
        "schema_version": 1,
        "extensions": extensions,
    });
    std::fs::write(hub_dir.join("index.json"), serde_json::to_vec_pretty(&index).unwrap()).unwrap();
}

fn write_extension_manifest(ext_dir: &std::path::Path, name: &str, version: &str) {
    std::fs::create_dir_all(ext_dir).unwrap();
    let manifest = json!({
        "name": name,
        "version": version,
    });
    std::fs::write(
        ext_dir.join("aion-extension.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// HM-1: Get extension list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm1_empty_index_returns_empty_list() {
    let h = setup();
    // No index.json at all.
    let list = h.index_manager.load_index().await;
    assert!(list.is_empty());
}

#[tokio::test]
async fn hm1_load_index_with_entries() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[
            json!({
                "name": "ext-alpha",
                "version": "1.0.0",
                "display_name": "Alpha Extension",
                "tags": ["tools"],
                "bundled": false,
                "curated": true,
            }),
            json!({
                "name": "ext-beta",
                "version": "2.0.0",
                "bundled": true,
            }),
        ],
    );

    let list = h.index_manager.load_index().await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].name, "ext-alpha");
    assert_eq!(list[0].status, HubExtensionStatus::NotInstalled);
    assert_eq!(list[1].name, "ext-beta");
    // Bundled always installed.
    assert_eq!(list[1].status, HubExtensionStatus::Installed);
}

// ---------------------------------------------------------------------------
// HM-2: Install extension
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm2_install_extension_with_valid_directory() {
    let h = setup();

    // Create index entry and extension directory.
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "my-ext",
            "version": "1.0.0",
            "curated": true,
        })],
    );
    write_extension_manifest(&h.hub_dir.path().join("my-ext"), "my-ext", "1.0.0");

    let result = h.installer.install("my-ext").await;
    assert!(result.success, "install should succeed: {:?}", result.msg);
}

// ---------------------------------------------------------------------------
// HM-3: Install failure (nonexistent extension)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm3_install_nonexistent_extension() {
    let h = setup();
    write_hub_index(h.hub_dir.path(), &[]);

    let result = h.installer.install("nonexistent-ext").await;
    assert!(!result.success);
    assert!(result.msg.is_some());
    assert!(result.msg.unwrap().contains("not found in hub index"));
}

#[tokio::test]
async fn hm3_install_no_directory() {
    let h = setup();
    // Extension is in the index but no directory exists.
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "remote-ext",
            "version": "1.0.0",
        })],
    );

    let result = h.installer.install("remote-ext").await;
    assert!(!result.success);
    assert!(result.msg.is_some());
    assert!(result.msg.unwrap().contains("not found"));
}

// ---------------------------------------------------------------------------
// HM-4: Retry install
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm4_retry_install_succeeds_after_directory_created() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "retry-ext",
            "version": "1.0.0",
            "curated": true,
        })],
    );

    // First attempt fails — no directory.
    let result = h.installer.retry_install("retry-ext").await;
    assert!(!result.success);

    // Create the directory, then retry.
    write_extension_manifest(&h.hub_dir.path().join("retry-ext"), "retry-ext", "1.0.0");

    let result = h.installer.retry_install("retry-ext").await;
    assert!(result.success);
}

// ---------------------------------------------------------------------------
// HM-5: Check updates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm5_check_updates_empty_when_no_installed() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "ext-a",
            "version": "2.0.0",
        })],
    );

    let updates = h.installer.check_updates().await;
    // No extensions installed → no updates.
    assert!(updates.is_empty());
}

#[tokio::test]
async fn hm5_check_updates_detects_newer_version() {
    let h = setup();

    // Create an extension and initialize the registry with it.
    let ext_dir = h.hub_dir.path().join("my-ext");
    write_extension_manifest(&ext_dir, "my-ext", "1.0.0");

    let scan_paths = vec![aionui_extension::loader::ScanPath {
        path: h.hub_dir.path().to_path_buf(),
        source: aionui_extension::types::ExtensionSource::Env,
    }];
    h.registry.initialize_with_scan_paths(scan_paths).await.unwrap();

    // Index has a newer version.
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "my-ext",
            "version": "2.0.0",
        })],
    );

    let updates = h.installer.check_updates().await;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].name, "my-ext");
    assert_eq!(updates[0].current_version, "1.0.0");
    assert_eq!(updates[0].latest_version, "2.0.0");
}

// ---------------------------------------------------------------------------
// HM-6: Update extension
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm6_update_extension() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "upd-ext",
            "version": "2.0.0",
            "curated": true,
        })],
    );
    // Extension directory exists with a valid manifest.
    write_extension_manifest(&h.hub_dir.path().join("upd-ext"), "upd-ext", "2.0.0");

    let result = h.installer.update("upd-ext").await;
    assert!(result.success, "update should succeed: {:?}", result.msg);
}

#[tokio::test]
async fn hm6_update_nonexistent_extension() {
    let h = setup();
    write_hub_index(h.hub_dir.path(), &[]);

    let result = h.installer.update("no-ext").await;
    assert!(!result.success);
}

// ---------------------------------------------------------------------------
// HM-7: Bundled extension status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm7_bundled_extensions_always_installed() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "bundled-ext",
            "version": "1.0.0",
            "bundled": true,
        })],
    );

    let list = h.index_manager.load_index().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, HubExtensionStatus::Installed);
    assert!(list[0].bundled);
}

// ---------------------------------------------------------------------------
// HM (extra): Uninstall extension
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hub_uninstall_removes_directory() {
    let h = setup();
    let ext_dir = h.hub_dir.path().join("remove-ext");
    write_extension_manifest(&ext_dir, "remove-ext", "1.0.0");

    assert!(ext_dir.exists());

    let result = h.installer.uninstall("remove-ext").await;
    assert!(result.success, "uninstall should succeed: {:?}", result.msg);
    assert!(!ext_dir.exists(), "extension directory should be removed");
}

#[tokio::test]
async fn hub_uninstall_nonexistent_returns_error() {
    let h = setup();
    let result = h.installer.uninstall("no-such-ext").await;
    assert!(!result.success);
    assert!(result.msg.unwrap().contains("not installed"));
}

// ---------------------------------------------------------------------------
// Verification tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_installation_catches_invalid_manifest() {
    let h = setup();
    let ext_dir = h.hub_dir.path().join("bad-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("aion-extension.json"), b"not valid json").unwrap();

    let result = h.installer.verify_installation(&ext_dir);
    assert!(result.is_err());
}

#[tokio::test]
async fn verify_installation_catches_reserved_name() {
    let h = setup();
    let ext_dir = h.hub_dir.path().join("aion-bad");
    write_extension_manifest(&ext_dir, "aion-bad", "1.0.0");

    let result = h.installer.verify_installation(&ext_dir);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Index status merge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn index_merge_shows_update_available() {
    let h = setup();

    // Initialize registry with v1.0.0.
    let ext_dir = h.hub_dir.path().join("status-ext");
    write_extension_manifest(&ext_dir, "status-ext", "1.0.0");

    let scan_paths = vec![aionui_extension::loader::ScanPath {
        path: h.hub_dir.path().to_path_buf(),
        source: aionui_extension::types::ExtensionSource::Env,
    }];
    h.registry.initialize_with_scan_paths(scan_paths).await.unwrap();

    // Index advertises v2.0.0.
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "status-ext",
            "version": "2.0.0",
        })],
    );

    let list = h.index_manager.load_index().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, HubExtensionStatus::UpdateAvailable);
}

#[tokio::test]
async fn index_merge_shows_installed_when_same_version() {
    let h = setup();

    let ext_dir = h.hub_dir.path().join("same-ext");
    write_extension_manifest(&ext_dir, "same-ext", "1.0.0");

    let scan_paths = vec![aionui_extension::loader::ScanPath {
        path: h.hub_dir.path().to_path_buf(),
        source: aionui_extension::types::ExtensionSource::Env,
    }];
    h.registry.initialize_with_scan_paths(scan_paths).await.unwrap();

    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "same-ext",
            "version": "1.0.0",
        })],
    );

    let list = h.index_manager.load_index().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, HubExtensionStatus::Installed);
}

// ---------------------------------------------------------------------------
// M9 E13 curated marketplace gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hm_curated_rejects_non_curated() {
    let h = setup();
    write_hub_index(
        h.hub_dir.path(),
        &[json!({
            "name": "wild-ext",
            "version": "1.0.0",
            "curated": false,
        })],
    );
    write_extension_manifest(&h.hub_dir.path().join("wild-ext"), "wild-ext", "1.0.0");

    let result = h.installer.install("wild-ext").await;
    assert!(!result.success);
    assert!(
        result.msg.unwrap().contains("not curated"),
        "expected curated gate error"
    );
}
