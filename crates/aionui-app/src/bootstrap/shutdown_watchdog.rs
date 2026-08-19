//! Hard shutdown watchdog (AIONUI-16).
//!
//! The data-dir instance `flock` is only released by the kernel when this
//! process exits (`aionui-db/src/instance_lock.rs`). Sentry evidence shows the
//! lock being held for 20–53 minutes across "clean quits", i.e. a shutdown
//! that began but never finished: the graceful tail contains awaits that are
//! unbounded by design (axum's connection drain waits for every connection
//! task with no timeout; `sqlx::Pool::close()` waits for all checked-out
//! connections; detached WebSocket tasks are never cancelled and can hold
//! pool connections forever).
//!
//! The individual stages are bounded with `tokio::time::timeout` in
//! `cmd_server`, but those bounds live inside the tokio runtime. If the
//! runtime wedges after the shutdown signal was observed (or a stage outside
//! the bounds hangs), nothing else guarantees process exit. This watchdog is
//! the last-resort bound for that window: a plain OS thread, armed when the
//! shutdown signal fires and disarmed once the graceful tail completes. If
//! the timeout elapses while armed, it logs the failure, emits the bootstrap
//! boundary stderr line, and force-exits so the instance lock is released in
//! bounded time.
//!
//! Coverage limit: arming happens inside the runtime (the graceful-shutdown
//! callback), and every shutdown-signal source is driven by the runtime too.
//! A runtime that wedges *before* the signal is observed therefore never arms
//! the watchdog, and that hang is not bounded by it.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::bootstrap::{BootstrapError, BootstrapErrorCode};
use crate::process_report::ExitKind;

/// Stage label shared by every watchdog log/error event.
const WATCHDOG_STAGE: &str = "shutdown.watchdog";

/// Exit code used on forced exit; `ExitKind::Internal` is the class
/// `BOOTSTRAP_SHUTDOWN_FAILED` maps to via `BootstrapError`.
const WATCHDOG_EXIT_CODE: i32 = ExitKind::Internal.raw_code() as i32;

enum WatchdogMsg {
    Arm,
    Disarm,
}

/// Cloneable handle controlling the watchdog thread. Dropping every handle
/// without arming (or after disarming) lets the thread exit without firing.
#[derive(Clone)]
pub(crate) struct ShutdownWatchdog {
    tx: mpsc::Sender<WatchdogMsg>,
}

impl ShutdownWatchdog {
    /// Spawn the production watchdog: on timeout it logs, prints the bootstrap
    /// stderr boundary line, and calls `std::process::exit`.
    pub(crate) fn spawn(timeout: Duration) -> Self {
        Self::spawn_with_action(timeout, move || {
            // Nothing before `std::process::exit` may be allowed to panic or
            // block the exit: on the ParentExit path the desktop parent is
            // already gone and its stdio pipes are closed, so `eprintln!`
            // would panic on EPIPE, unwind this thread, and the forced exit —
            // the entire point of the watchdog — would never run.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let timeout_secs = timeout.as_secs();
                tracing::error!(
                    code = BootstrapErrorCode::ShutdownFailed.as_str(),
                    stage = WATCHDOG_STAGE,
                    timeout_secs,
                    "graceful shutdown did not complete in time; forcing exit to release the instance lock"
                );
                let error = BootstrapError::new(
                    BootstrapErrorCode::ShutdownFailed,
                    WATCHDOG_STAGE,
                    "graceful shutdown did not complete in time; forced exit",
                )
                .with_field("timeout_secs", timeout_secs.to_string());
                // `std::process::exit` skips destructors, so the buffered
                // tracing writer may not flush. The stderr boundary line is
                // written directly (best-effort: a closed pipe returns an
                // error instead of panicking) and survives the forced exit.
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "{}", error.stderr_line());
            }));
            std::process::exit(WATCHDOG_EXIT_CODE);
        })
    }

    /// Spawn a watchdog with an injectable timeout action (used by tests; the
    /// production action force-exits the process).
    pub(crate) fn spawn_with_action(timeout: Duration, on_timeout: impl FnOnce() + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel::<WatchdogMsg>();
        if let Err(error) = std::thread::Builder::new()
            .name("shutdown-watchdog".into())
            .spawn(move || watchdog_thread(rx, timeout, on_timeout))
        {
            // Thread spawn failure leaves shutdown unbounded, exactly as
            // before this watchdog existed; the stage timeouts in cmd_server
            // still bound the common paths. Log it so a later unbounded hang
            // is attributable to the missing watchdog instead of looking like
            // an armed watchdog that mysteriously never fired.
            tracing::warn!(
                code = "BOOTSTRAP_DEGRADED_SHUTDOWN_WATCHDOG",
                stage = WATCHDOG_STAGE,
                error = %error,
                "failed to spawn shutdown watchdog thread; shutdown tail has stage bounds only"
            );
        }
        Self { tx }
    }

    /// Start the countdown. Called when the shutdown signal fires. Returns
    /// `false` when the watchdog thread is not running (spawn failed), so the
    /// caller can log the degraded state instead of claiming an armed bound.
    pub(crate) fn arm(&self) -> bool {
        self.tx.send(WatchdogMsg::Arm).is_ok()
    }

    /// Cancel the countdown. Called once the graceful tail has completed.
    pub(crate) fn disarm(&self) {
        let _ = self.tx.send(WatchdogMsg::Disarm);
    }
}

fn watchdog_thread(rx: mpsc::Receiver<WatchdogMsg>, timeout: Duration, on_timeout: impl FnOnce() + Send + 'static) {
    // Unarmed: wait indefinitely for the first arm. Sender drop (process
    // exiting without a shutdown signal, e.g. bootstrap failure) ends the
    // thread without firing.
    loop {
        match rx.recv() {
            Ok(WatchdogMsg::Arm) => break,
            Ok(WatchdogMsg::Disarm) => continue,
            Err(mpsc::RecvError) => return,
        }
    }

    // Armed: fire unless disarmed before the deadline. The deadline is fixed
    // at first arm; duplicate arms do not extend it.
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            on_timeout();
            return;
        }
        match rx.recv_timeout(remaining) {
            Ok(WatchdogMsg::Disarm) => return,
            Ok(WatchdogMsg::Arm) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                on_timeout();
                return;
            }
            // All handles dropped while armed without an explicit disarm:
            // the graceful tail owns a handle for its whole duration, so this
            // means the process is already tearing down normally.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::ShutdownWatchdog;

    fn flag_action(flag: &Arc<AtomicBool>) -> impl FnOnce() + Send + 'static {
        let flag = Arc::clone(flag);
        move || flag.store(true, Ordering::SeqCst)
    }

    fn wait_for(flag: &AtomicBool, deadline: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if flag.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        flag.load(Ordering::SeqCst)
    }

    #[test]
    fn fires_when_armed_and_not_disarmed() {
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = ShutdownWatchdog::spawn_with_action(Duration::from_millis(50), flag_action(&fired));

        watchdog.arm();

        assert!(
            wait_for(&fired, Duration::from_secs(5)),
            "watchdog must fire after the timeout when never disarmed"
        );
    }

    #[test]
    fn does_not_fire_when_disarmed_in_time() {
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = ShutdownWatchdog::spawn_with_action(Duration::from_millis(200), flag_action(&fired));

        watchdog.arm();
        watchdog.disarm();

        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !fired.load(Ordering::SeqCst),
            "watchdog must not fire after an in-time disarm"
        );
    }

    #[test]
    fn does_not_fire_when_never_armed() {
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = ShutdownWatchdog::spawn_with_action(Duration::from_millis(50), flag_action(&fired));

        drop(watchdog);

        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !fired.load(Ordering::SeqCst),
            "watchdog must not fire when the shutdown signal never arrived"
        );
    }

    #[test]
    fn duplicate_arm_does_not_extend_the_deadline() {
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = ShutdownWatchdog::spawn_with_action(Duration::from_millis(1000), flag_action(&fired));

        let start = std::time::Instant::now();
        watchdog.arm();
        std::thread::sleep(Duration::from_millis(500));
        watchdog.arm();

        assert!(
            wait_for(&fired, Duration::from_secs(5)),
            "watchdog must fire despite a duplicate arm"
        );
        // A deadline-resetting regression would fire at ~1500ms from the first
        // arm; the fixed deadline fires at ~1000ms. The 1400ms bound separates
        // the two with scheduling slack on both sides.
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1400),
            "watchdog deadline is fixed at first arm; a duplicate arm must not reset it (fired after {elapsed:?})"
        );
    }

    #[test]
    fn arm_reports_whether_the_watchdog_thread_is_alive() {
        let fired = Arc::new(AtomicBool::new(false));
        let watchdog = ShutdownWatchdog::spawn_with_action(Duration::from_millis(50), flag_action(&fired));
        assert!(watchdog.arm(), "arm must report success while the thread is alive");

        // Simulate a dead watchdog thread (e.g. spawn failure dropped the
        // receiver): arm must report the degradation instead of no-opping.
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let dead = ShutdownWatchdog { tx };
        assert!(!dead.arm(), "arm must report failure when the watchdog thread is gone");
    }
}
