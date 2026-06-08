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
