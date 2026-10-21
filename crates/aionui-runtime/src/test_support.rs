#[path = "../tests/support/native_fixture.rs"]
pub(crate) mod native_fixture;

/// The fixture follows the platform's distribution layout without depending
/// on an installed Node version. Commands are real native executables.
pub(crate) fn write_node_runtime(root: &std::path::Path) -> std::path::PathBuf {
    let node = root.join(if cfg!(windows) { "node.exe" } else { "bin/node" });
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::copy(native_fixture::executable(), &node).unwrap();
    let npm_bin = root.join(if cfg!(windows) {
        "node_modules/npm/bin"
    } else {
        "lib/node_modules/npm/bin"
    });
    std::fs::create_dir_all(&npm_bin).unwrap();
    for script in ["npm-cli.js", "npx-cli.js"] {
        std::fs::write(npm_bin.join(script), b"// Native fixture entrypoint marker\n").unwrap();
    }
    node
}

pub(crate) fn run_in_env_child<F>(test_name: &str, configure: F) -> bool
where
    F: FnOnce(&mut std::process::Command),
{
    const CHILD_ENV: &str = "AIONUI_RUNTIME_ENV_TEST_CHILD";

    if std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }

    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .env(CHILD_ENV, "1");
    configure(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child test failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}
