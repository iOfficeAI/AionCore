use super::*;

fn write_file(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, b"").expect("write file");
}

#[tokio::test]
async fn managed_runtime_validation_uses_real_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-test");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    let node = bin.join("node");
    std::fs::write(&node, "#!/bin/sh\necho v24.11.0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&node).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&node, perms).unwrap();
    }

    let err = validate_managed_runtime(&root, None).await.unwrap_err();
    assert!(err.to_string().to_ascii_lowercase().contains("npm"));
}

#[test]
fn managed_runtime_support_reports_current_platform() {
    let support = probe_support();
    let expected = cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(windows);
    assert_eq!(support.supported, expected);
}

#[test]
fn classify_error_detects_bundled_node_runtime_missing() {
    let err = NodeRuntimeError::managed_invalid(
        "bundled Node runtime missing under C:\\Program Files\\AionUi\\resources\\bundled-aioncore\\win32-x64\\managed-resources\\node\\node-v24.11.0-win-x64",
    );
    let (kind, status) = classify_error(&err);

    assert_eq!(kind, NodeRuntimeFailureKind::BundledResourceMissing);
    assert_eq!(status, None);
}

#[tokio::test]
async fn bundled_runtime_missing_reports_bundled_resource_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let bundled_root = tmp.path().join("bundled");
    if !crate::test_support::run_in_env_child(
        "node_runtime::managed::tests::bundled_runtime_missing_reports_bundled_resource_missing",
        |command| {
            command.env("AIONUI_BUNDLED_MANAGED_RESOURCES", &bundled_root);
        },
    ) {
        return;
    }

    crate::cache::init(tmp.path().join("data"));
    managed_resources::set_managed_resources_mode(managed_resources::ManagedResourcesMode::Bundled);

    let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let reporter_updates = updates.clone();
    let reporter = move |update: NodeRuntimeProgress| {
        reporter_updates.lock().unwrap().push(update);
    };

    let result = install_and_validate_with_reporter(Some(&reporter)).await;
    managed_resources::set_managed_resources_mode(managed_resources::ManagedResourcesMode::Download);

    let error = result.expect_err("missing bundled runtime should fail");
    assert!(error.to_string().contains("bundled Node runtime missing"));
    let updates = updates.lock().unwrap();
    assert!(updates.iter().any(|update| {
        update.phase == crate::NodeRuntimeProgressPhase::Failed
            && update.failure_kind == Some(NodeRuntimeFailureKind::BundledResourceMissing)
    }));
}

#[test]
fn managed_runtime_install_lock_path_uses_runtime_root() {
    let root = PathBuf::from("/tmp/aionui/runtime/node");
    assert_eq!(install_lock_path(&root), root.join("node-runtime-install.lock"));
}

#[test]
fn managed_runtime_timeout_error_is_explicit() {
    let error = timeout_error(
        "download archive",
        "https://example.com/node.tar.gz",
        MANAGED_NODE_DOWNLOAD_TIMEOUT,
    );
    let message = error.to_string();
    assert!(message.contains("download archive timed out"));
    assert!(message.contains("600s"));
}

#[test]
fn managed_runtime_http_status_error_is_explicit() {
    let error = http_status_error(
        "download archive",
        "https://example.com/node.tar.gz",
        reqwest::StatusCode::BAD_GATEWAY,
    );
    let message = error.to_string();
    assert!(message.contains("HTTP 502"));
    assert!(message.contains("download archive"));
}

#[test]
fn managed_runtime_official_source_uses_nodejs_org() {
    let source = ManagedNodeDownloadSource::official(PlatformSpec {
        folder_suffix: "darwin-arm64",
        archive_ext: "tar.gz",
        runtime_key: "darwin-arm64",
        executable: "bin/node",
    });

    assert_eq!(source.source, "nodejs.org");
    assert_eq!(
        source.url,
        "https://nodejs.org/dist/v24.11.0/node-v24.11.0-darwin-arm64.tar.gz"
    );
    assert_eq!(source.sha256, None);
}

#[test]
fn managed_node_contract_uses_runtime_key_and_exported_root() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle_root = tmp.path().join("managed-resources");
    let exported = bundle_root.join("node").join("node-v24.11.0-darwin-arm64");
    std::fs::create_dir_all(exported.join("bin")).unwrap();
    write_file(&exported.join("bin").join("node"));

    let spec = PlatformSpec {
        folder_suffix: "darwin-arm64",
        archive_ext: "tar.gz",
        runtime_key: "darwin-arm64",
        executable: "bin/node",
    };

    let contract = managed_node_contract_for_export_with_spec(&bundle_root, &exported, spec).expect("contract");

    assert_eq!(contract.version, "24.11.0");
    assert_eq!(contract.root, "node/node-v24.11.0-darwin-arm64");
    assert_eq!(contract.executable, "bin/node");
}

#[test]
fn managed_runtime_checksum_verification_detects_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("node.tar.gz");
    std::fs::write(&path, b"not-node").unwrap();

    let error = verify_archive_checksum(&path, "deadbeef").unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
}

#[test]
fn managed_runtime_injects_npm_state_under_runtime_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-test");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("node"), b"").unwrap();
    std::fs::write(bin.join("npm"), b"").unwrap();
    std::fs::write(bin.join("npx"), b"").unwrap();

    let runtime = runtime_from_root(&root, ResolvedNodeSource::Managed).expect("runtime");
    let env: std::collections::HashMap<_, _> = runtime
        .npm_command()
        .env
        .into_iter()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();

    assert_eq!(
        env.get("npm_config_cache"),
        Some(&root.join("cache").display().to_string())
    );
    assert_eq!(
        env.get("npm_config_userconfig"),
        Some(&root.join("blank_user_npmrc").display().to_string())
    );
    assert_eq!(
        env.get("npm_config_globalconfig"),
        Some(&root.join("blank_global_npmrc").display().to_string())
    );
    assert_eq!(
        env.get("npm_config_prefix"),
        Some(&root.join("tools").join("global").display().to_string())
    );
}

#[tokio::test]
async fn bundled_runtime_validation_failure_does_not_fallback_to_remote_download() {
    let tmp = tempfile::tempdir().unwrap();
    let bundled_root = tmp.path().join("bundled");
    if !crate::test_support::run_in_env_child(
        "node_runtime::managed::tests::bundled_runtime_validation_failure_does_not_fallback_to_remote_download",
        |command| {
            command.env("AIONUI_BUNDLED_MANAGED_RESOURCES", &bundled_root);
        },
    ) {
        return;
    }
    let bundled_root = std::path::PathBuf::from(std::env::var_os("AIONUI_BUNDLED_MANAGED_RESOURCES").unwrap());
    let runtime_root = bundled_root.join("node").join("node-v24.11.0-darwin-arm64");
    let bin = runtime_root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    let node = bin.join("node");
    std::fs::write(&node, "#!/bin/sh\necho v24.11.0\n").unwrap();
    let npm = bin.join("npm");
    std::fs::write(&npm, "#!/bin/sh\nexit 1\n").unwrap();
    let npx = bin.join("npx");
    std::fs::write(&npx, "#!/bin/sh\nexit 1\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [&node, &npm, &npx] {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    managed_resources::set_managed_resources_mode(managed_resources::ManagedResourcesMode::Bundled);
    let runtime_root = tmp.path().join("runtime").join("node");
    std::fs::create_dir_all(&runtime_root).unwrap();
    let result = activate_local_runtime_source(
        &runtime_root,
        PlatformSpec {
            folder_suffix: "darwin-arm64",
            archive_ext: "tar.gz",
            runtime_key: "darwin-arm64",
            executable: "bin/node",
        },
        None,
    )
    .await;
    managed_resources::set_managed_resources_mode(managed_resources::ManagedResourcesMode::Download);

    let error = result.expect_err("bundled validation failure should abort");
    assert!(error.to_string().contains("bundled Node runtime failed validation"));
}

#[test]
fn windows_managed_cli_paths_use_node_modules_under_archive_root() {
    let root = PathBuf::from(r"C:\AionUi\node-v24.11.0-win-x64");
    let npm = managed_npm_cli_path_for_layout(&root, ManagedNodeArchiveLayout::Windows);
    let npx = managed_npx_cli_path_for_layout(&root, ManagedNodeArchiveLayout::Windows);

    assert_eq!(
        npm,
        root.join("node_modules").join("npm").join("bin").join("npm-cli.js")
    );
    assert_eq!(
        npx,
        root.join("node_modules").join("npm").join("bin").join("npx-cli.js")
    );
}

#[test]
fn unix_managed_cli_paths_use_lib_node_modules() {
    let root = PathBuf::from("/opt/aionui/node-v24.11.0-darwin-arm64");
    let npm = managed_npm_cli_path_for_layout(&root, ManagedNodeArchiveLayout::Unix);
    let npx = managed_npx_cli_path_for_layout(&root, ManagedNodeArchiveLayout::Unix);

    assert_eq!(
        npm,
        root.join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js")
    );
    assert_eq!(
        npx,
        root.join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npx-cli.js")
    );
}

#[test]
fn windows_managed_runtime_prefers_direct_cli_entrypoints_over_wrappers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-win-x64");
    std::fs::create_dir_all(&root).unwrap();

    let node = root.join("node.exe");
    write_file(&node);
    write_file(&root.join("npm.cmd"));
    write_file(&root.join("npx.cmd"));
    let npm_cli = root.join("node_modules").join("npm").join("bin").join("npm-cli.js");
    let npx_cli = root.join("node_modules").join("npm").join("bin").join("npx-cli.js");
    write_file(&npm_cli);
    write_file(&npx_cli);

    let runtime = runtime_from_root_for_layout(&root, ResolvedNodeSource::Managed, ManagedNodeArchiveLayout::Windows)
        .expect("runtime should resolve");

    assert_eq!(runtime.npm_path, node);
    assert_eq!(runtime.npm_args_prefix, vec![npm_cli.into_os_string()]);
    assert_eq!(runtime.npx_path, root.join("node.exe"));
    assert_eq!(runtime.npx_args_prefix, vec![npx_cli.into_os_string()]);

    let npx_env: std::collections::HashMap<_, _> = runtime
        .npx_command()
        .env
        .into_iter()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
        .collect();
    assert!(npx_env.contains_key("npm_config_cache"));
    assert!(npx_env.contains_key("npm_config_userconfig"));
    assert!(npx_env.contains_key("npm_config_globalconfig"));
    assert!(npx_env.contains_key("npm_config_prefix"));
}

#[test]
fn windows_managed_runtime_falls_back_to_wrappers_when_direct_cli_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-win-x64");
    std::fs::create_dir_all(&root).unwrap();

    write_file(&root.join("node.exe"));
    write_file(&root.join("npm.cmd"));
    write_file(&root.join("npx.cmd"));

    let runtime = runtime_from_root_for_layout(&root, ResolvedNodeSource::Managed, ManagedNodeArchiveLayout::Windows)
        .expect("runtime should resolve");

    assert_eq!(runtime.npm_path, root.join("npm.cmd"));
    assert!(runtime.npm_args_prefix.is_empty());
    assert_eq!(runtime.npx_path, root.join("npx.cmd"));
    assert!(runtime.npx_args_prefix.is_empty());
}

#[test]
fn unix_managed_runtime_keeps_wrapper_entrypoints_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-darwin-arm64");
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    write_file(&bin.join("node"));
    write_file(&bin.join("npm"));
    write_file(&bin.join("npx"));
    write_file(
        &root
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npm-cli.js"),
    );
    write_file(
        &root
            .join("lib")
            .join("node_modules")
            .join("npm")
            .join("bin")
            .join("npx-cli.js"),
    );

    let runtime = runtime_from_root_for_layout(&root, ResolvedNodeSource::Managed, ManagedNodeArchiveLayout::Unix)
        .expect("runtime should resolve");

    assert_eq!(runtime.npm_path, bin.join("npm"));
    assert!(runtime.npm_args_prefix.is_empty());
    assert_eq!(runtime.npx_path, bin.join("npx"));
    assert!(runtime.npx_args_prefix.is_empty());
}

#[test]
fn windows_managed_runtime_fails_when_entrypoints_are_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("node-v24.11.0-win-x64");
    std::fs::create_dir_all(&root).unwrap();
    write_file(&root.join("node.exe"));

    let error = runtime_from_root_for_layout(&root, ResolvedNodeSource::Managed, ManagedNodeArchiveLayout::Windows)
        .expect_err("missing npm should fail");
    assert!(
        error.to_string().contains("managed npm entrypoint missing"),
        "unexpected error: {error}"
    );

    let npm_cli = root.join("node_modules").join("npm").join("bin").join("npm-cli.js");
    write_file(&npm_cli);

    let error = runtime_from_root_for_layout(&root, ResolvedNodeSource::Managed, ManagedNodeArchiveLayout::Windows)
        .expect_err("missing npx should fail");
    assert!(
        error.to_string().contains("managed npx entrypoint missing"),
        "unexpected error: {error}"
    );
}
