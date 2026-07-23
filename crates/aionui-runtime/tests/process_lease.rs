use std::time::Duration;

use aionui_runtime::{Builder, ProcessLeaseSpec};

#[tokio::test]
async fn lease_tracks_heartbeat_acceptance_and_timeout() {
    let mut command = Builder::new("sh");
    command.args(["-c", "sleep 60"]);
    let mut child = command
        .spawn_leased(ProcessLeaseSpec::new(
            "lease-runtime",
            "host:local",
            Duration::from_millis(75),
        ))
        .unwrap();

    assert!(child.lease().accepts_work());
    let initial_heartbeat = child.lease().last_heartbeat_at();
    tokio::time::sleep(Duration::from_millis(5)).await;
    child.lease().heartbeat();
    assert!(child.lease().last_heartbeat_at() >= initial_heartbeat);

    child.lease().stop_accepting_work();
    assert!(!child.lease().accepts_work());
    let exit = child.wait_with_timeout().await.unwrap();
    assert!(exit.timed_out);
    assert!(exit.status.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn termination_kills_the_complete_process_tree() {
    let directory = tempfile::tempdir().unwrap();
    let pid_file = directory.path().join("grandchild.pid");
    let script = format!("sleep 60 & echo $! > '{}'; wait", pid_file.display());
    let mut command = Builder::new("sh");
    command.args(["-c", &script]);
    let mut child = command
        .spawn_leased(ProcessLeaseSpec::new(
            "lease-tree",
            "host:local",
            Duration::from_secs(30),
        ))
        .unwrap();

    let grandchild_pid = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = value.trim().parse::<i32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    child.terminate_tree().await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let exists = unsafe { libc::kill(grandchild_pid, 0) } == 0;
            let running = if cfg!(target_os = "linux") {
                std::fs::read_to_string(format!("/proc/{grandchild_pid}/stat"))
                    .ok()
                    .and_then(|stat| stat.split_whitespace().nth(2).map(str::to_owned))
                    .is_some_and(|state| state != "Z")
            } else {
                exists
            };
            if !running {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(stopped, "grandchild process {grandchild_pid} survived lease cleanup");
}

#[cfg(unix)]
#[test]
fn port_holder_child() {
    let Ok(port) = std::env::var("AIONUI_TEST_PORT_HOLDER") else {
        return;
    };
    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    loop {
        let _ = listener.accept();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn terminating_a_lease_releases_ports_owned_by_its_process() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let mut command = Builder::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "port_holder_child", "--nocapture"])
        .env("AIONUI_TEST_PORT_HOLDER", port.to_string());
    let mut child = command
        .spawn_leased(ProcessLeaseSpec::new(
            "lease-port",
            "host:local",
            Duration::from_secs(30),
        ))
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    child.terminate_tree().await.unwrap();
    let rebound = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
                break listener;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(rebound.local_addr().unwrap().port(), port);
}
