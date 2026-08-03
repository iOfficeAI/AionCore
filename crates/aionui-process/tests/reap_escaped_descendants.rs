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
/// `set -m` (job control) is what makes the background job a group leader —
/// without it the child inherits the outer's group, the group kill reaches it
/// anyway, and the test would pass while proving nothing. Verified below by
/// asserting the two pgids actually differ.
///
/// Returns (outer pid, escaped child pid).
fn spawn_escaping_tree(marker: &str) -> (u32, u32) {
    // `setsid` is not a binary on macOS, so the outer's own session is created
    // from Rust via pre_exec rather than a helper.
    use std::os::unix::process::CommandExt;

    let script = format!("set -m; sleep 600 & echo $! > {marker}; sleep 600");
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

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs error checking only; it never delivers a signal.
    unsafe { libc::kill(pid as libc::c_int, 0) == 0 }
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
    assert_ne!(
        pgid(outer),
        pgid(inner),
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
