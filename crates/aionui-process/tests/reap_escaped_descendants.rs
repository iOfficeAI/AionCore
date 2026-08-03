//! A child that leaves the process group must still be torn down.
//!
//! Measured motivation (agy 1.1.9): its tool subprocesses each become their own
//! process-group leader, so the group SIGKILL that removes the CLI leaves the
//! tool running, reparented to init. A `cargo build` or dev server started by a
//! cancelled turn then had nothing left to stop it.

use aionui_process::{Containment, ContainmentKillOutcome, ProcessGroupContainment};
use std::process::{Command, Stdio};

/// Reproduce the measured agy shape: an outer process in its own group, whose
/// child puts ITSELF in a different group and so survives a group kill.
///
/// The child must put ITSELF in a new session; inheriting the outer's group
/// would let the plain group kill reach it and the test would pass while
/// proving nothing. The pgids are asserted to differ for exactly that reason.
///
/// `set -m` was tried first and is NOT portable: on Linux `/bin/sh` is
/// typically dash, whose background jobs stay in the caller's process group,
/// so the fixture silently stopped escaping and the guard assertion caught it
/// on CI. `setsid(1)` exists on Linux but not macOS, and perl's `POSIX::setsid`
/// covers macOS — so whichever is present is used, and the guard still decides
/// whether the fixture is worth anything.
///
/// Returns (outer pid, escaped child pid).
fn spawn_escaping_tree(marker: &str) -> (u32, u32) {
    // The outer's own session is created from Rust via pre_exec; only the inner
    // one needs a helper, because it is the shell that spawns it.
    use std::os::unix::process::CommandExt;

    let detach = if which("setsid") {
        "setsid".to_owned()
    } else if which("perl") {
        "perl -e 'use POSIX; POSIX::setsid(); exec @ARGV'".to_owned()
    } else {
        panic!("need setsid(1) or perl to build a process that leaves its group");
    };
    let script = format!("{detach} sleep 600 & echo $! > {marker}; sleep 600");
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and is the documented way to detach
    // into a new session between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn escaping tree");
    let outer = child.id();

    // Reap the outer as soon as it dies, the way `ManagedProcess`'s exit
    // monitor does in production. Without a waiter the killed process lingers
    // as a zombie, `kill(pid, 0)` still succeeds on it, and both this test's
    // liveness checks and `process_group_alive` would read it as running.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    // Wait for the marker file so the grandchild exists before we snapshot.
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(marker)
            && let Ok(pid) = s.trim().parse::<u32>()
        {
            return (outer, pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("grandchild never reported its pid");
}

fn which_path(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
    })
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs error checking only; it never delivers a signal.
    unsafe { libc::kill(pid as libc::c_int, 0) == 0 }
}

/// Wait until `child` is in a different process group from `parent`.
///
/// The detach happens INSIDE the child (setsid after fork, before exec), so the
/// pid is published by the shell before the new group exists. Comparing the two
/// immediately is a race: it passed by luck and failed under load. Returns
/// false if the split never happens, so the caller still fails loudly on a
/// fixture that genuinely does not escape.
fn wait_until_group_split(parent: u32, child: u32) -> bool {
    for _ in 0..200 {
        if pgid(parent) != pgid(child) && pgid(child) > 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    false
}

fn pgid(pid: u32) -> i32 {
    // SAFETY: getpgid on a pid we spawned; -1 on failure is handled by callers.
    unsafe { libc::getpgid(pid as libc::c_int) }
}

#[test]
fn a_tool_child_that_left_the_group_is_still_reaped() {
    let marker = std::env::temp_dir().join(format!("aionui-reap-{}.pid", std::process::id()));
    let marker_s = marker.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&marker);

    let (outer, inner) = spawn_escaping_tree(&marker_s);
    assert!(alive(outer) && alive(inner), "tree did not start");

    // Without this the fixture is worthless: if the child shared the outer's
    // group, the plain group kill would remove it and the test would pass with
    // the reap deleted. An earlier version of this file did exactly that.
    assert!(
        wait_until_group_split(outer, inner),
        "fixture did not escape the group; the test would prove nothing"
    );

    // The outer made itself a session leader, so its group is its own pid.
    let containment = ProcessGroupContainment::new(outer, Some(outer));
    let outcome = containment.kill_all().expect("kill_all");

    // Give the kernel a moment to finish delivering the signals.
    for _ in 0..80 {
        if !alive(outer) && !alive(inner) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    let _ = std::fs::remove_file(&marker);
    // Belt and braces: never leave the fixture behind if the assert fails.
    let leaked = alive(inner);
    if leaked {
        // SAFETY: killing a pid this test itself created.
        unsafe { libc::kill(inner as libc::c_int, libc::SIGKILL) };
    }

    assert!(!alive(outer), "the contained process itself survived");
    assert!(
        !leaked,
        "the grandchild in its own session survived — this is the leak the reap exists to close"
    );
    assert_eq!(
        outcome,
        ContainmentKillOutcome::ProbedGone,
        "with the tree confirmed gone the outcome must not claim degradation"
    );
}

#[test]
fn an_unrelated_process_is_left_running() {
    // The reap walks a live parent table; a bug there would take out processes
    // that merely happened to be running. This is the guard against that.
    let marker = std::env::temp_dir().join(format!("aionui-bystander-{}.pid", std::process::id()));
    let marker_s = marker.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&marker);

    let (bystander_outer, bystander_inner) = spawn_escaping_tree(&marker_s);

    // Contain something that is not their ancestor: this very test process's
    // pid would be, so use a pid with no relationship at all — the bystander's
    // own grandchild, contained alone.
    let containment = ProcessGroupContainment::new(bystander_inner, Some(bystander_inner));
    let _ = containment.kill_all();

    std::thread::sleep(std::time::Duration::from_millis(200));
    let outer_survived = alive(bystander_outer);

    // SAFETY: cleaning up pids this test created.
    unsafe {
        libc::kill(bystander_outer as libc::c_int, libc::SIGKILL);
        libc::kill(bystander_inner as libc::c_int, libc::SIGKILL);
    }
    let _ = std::fs::remove_file(&marker);

    assert!(
        outer_survived,
        "killing a leaf must not walk upwards and take out its parent"
    );
}

/// The path a user's Cancel actually takes.
///
/// The first version of this fix lived in `ProcessGroupContainment::kill_all`,
/// which has NO caller in production code — the only thing invoking it was the
/// test above. Cancel goes through `ManagedProcess::kill`, so the fix was
/// green in tests and absent in reality; driving a real agy turn through the
/// HTTP Cancel still leaked `sleep 600` under init. This test pins the path
/// that matters.
#[tokio::test]
async fn managed_process_kill_reaps_a_child_that_left_the_group() {
    use aionui_common::CommandSpec;
    use aionui_process::ManagedProcess;

    let marker = std::env::temp_dir().join(format!("aionui-mpkill-{}.pid", std::process::id()));
    let marker_s = marker.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&marker);

    // ABSOLUTE paths: `ManagedProcess` spawns through the runtime's cleaned
    // environment, so a helper resolved from the test's own PATH may simply not
    // be found in the child — the detach would silently no-op and the fixture
    // would stop escaping.
    let detach = match (which_path("setsid"), which_path("perl")) {
        (Some(p), _) => p.display().to_string(),
        (None, Some(p)) => format!("{} -e 'use POSIX; POSIX::setsid(); exec @ARGV'", p.display()),
        _ => panic!("need setsid(1) or perl"),
    };
    let proc = ManagedProcess::spawn(
        CommandSpec {
            command: "/bin/sh".into(),
            args: vec![
                "-c".to_owned(),
                format!("{detach} sleep 600 & echo $! > {marker_s}; sleep 600"),
            ],
            env: Vec::new(),
            cwd: None,
        },
        &[],
    )
    .await
    .expect("spawn");

    let mut inner = 0u32;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if let Ok(s) = std::fs::read_to_string(&marker)
            && let Ok(pid) = s.trim().parse::<u32>()
        {
            inner = pid;
            break;
        }
    }
    assert_ne!(inner, 0, "grandchild never reported its pid");
    assert!(
        wait_until_group_split(proc.pid(), inner),
        "fixture did not escape the group; the test would prove nothing"
    );

    proc.kill(std::time::Duration::from_millis(200)).await.expect("kill");
    for _ in 0..80 {
        if !alive(inner) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let _ = std::fs::remove_file(&marker);
    let leaked = alive(inner);
    if leaked {
        // SAFETY: cleaning up a pid this test created.
        unsafe { libc::kill(inner as libc::c_int, libc::SIGKILL) };
    }
    assert!(!leaked, "ManagedProcess::kill left the escaped child running");
}
