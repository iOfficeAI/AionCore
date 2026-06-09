use std::future::Future;
use std::pin::Pin;

pub(crate) type ParentExitSignal = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub(crate) fn parent_exit_signal(parent_pid: Option<u32>) -> Option<ParentExitSignal> {
    #[cfg(windows)]
    {
        parent_pid.map(|pid| Box::pin(wait_for_parent_exit(pid)) as ParentExitSignal)
    }

    #[cfg(not(windows))]
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
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE, WaitForSingleObject,
    };

    unsafe {
        let handle = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, parent_pid);
        if handle.is_null() {
            return;
        }

        let wait_result = WaitForSingleObject(handle, INFINITE);
        let _ = CloseHandle(handle);
        if wait_result != WAIT_OBJECT_0 {
            return;
        }
    }
}
