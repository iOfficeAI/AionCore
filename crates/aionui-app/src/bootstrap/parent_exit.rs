use std::future::Future;
use std::pin::Pin;

pub(crate) type ParentExitSignal = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub(crate) fn parent_exit_signal(parent_pid: Option<u32>) -> Option<ParentExitSignal> {
    #[cfg(any(windows, unix))]
    {
        parent_pid.map(|pid| Box::pin(wait_for_parent_exit(pid)) as ParentExitSignal)
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = parent_pid;
        None
    }
}

#[cfg(windows)]
async fn wait_for_parent_exit(parent_pid: u32) {
    let (tx, rx) = tokio::sync::oneshot::channel();

    std::thread::spawn(move || {
        wait_for_parent_exit_blocking(parent_pid);
        let _ = tx.send(());
    });

    let _ = rx.await;
}

#[cfg(windows)]
fn wait_for_parent_exit_blocking(parent_pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid);
        if handle.is_null() {
            return;
        }

        // Any wait completion ends the parent-exit monitor; there is no
        // distinct error path because callers only need a shutdown signal.
        let _ = WaitForSingleObject(handle, INFINITE);
        let _ = CloseHandle(handle);
    }
}

#[cfg(unix)]
async fn wait_for_parent_exit(parent_pid: u32) {
    while current_parent_pid() == Some(parent_pid) {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[cfg(unix)]
fn current_parent_pid() -> Option<u32> {
    let parent_pid = unsafe { libc::getppid() };
    u32::try_from(parent_pid).ok().filter(|pid| *pid != 0)
}

#[cfg(test)]
mod tests {
    use super::parent_exit_signal;

    #[test]
    fn parent_exit_signal_is_absent_without_parent_pid() {
        assert!(parent_exit_signal(None).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_parent_exit_signal_resolves_when_parent_relationship_is_missing() {
        let signal = parent_exit_signal(Some(std::process::id())).expect("unix parent pid should produce a monitor");

        tokio::time::timeout(std::time::Duration::from_millis(50), signal)
            .await
            .expect("signal should resolve when the current parent no longer matches");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_parent_exit_signal_waits_while_parent_still_matches() {
        let signal = parent_exit_signal(Some(unsafe { libc::getppid() as u32 }))
            .expect("unix parent pid should produce a monitor");

        tokio::time::timeout(std::time::Duration::from_millis(50), signal)
            .await
            .expect_err("signal should not resolve while the current parent still matches");
    }
}
