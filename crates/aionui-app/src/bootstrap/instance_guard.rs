//! Bounded wait for the data-dir process-level instance guard (Sentry 135525166
//! Option A).
//!
//! When `DataDirInstanceGuard::try_acquire` reports the lock is held by a peer
//! (`Ok(None)`), a crash-orphan backend may still be shutting down via its
//! `--parent-pid` monitor and about to release the guard. We poll for a short,
//! fixed window before giving up so a losing instance yields cleanly instead of
//! racing. `Err` (flock unavailable) is propagated so the caller can decide to
//! proceed and rely on the bootstrap concurrency safety (Option B).

use std::path::Path;
use std::time::Duration;

use aionui_db::DataDirInstanceGuard;

/// Number of acquisition attempts before yielding, and the delay between them.
/// ~10 × 200ms ≈ 2s — enough for a crash-orphan to self-exit via its ~250ms
/// parent-exit poll and release the guard.
const WAIT_MAX_ATTEMPTS: u32 = 10;
const WAIT_DELAY: Duration = Duration::from_millis(200);

/// Poll `try_acquire` up to a fixed bound, yielding if a peer keeps the guard.
///
/// - `Ok(Some(guard))` — acquired within the window (winner).
/// - `Ok(None)` — a peer still owns the data dir after the bounded wait (loser).
/// - `Err(e)` — flock is unavailable; the caller proceeds without the guard.
pub(crate) fn wait_for_instance_guard(db_path: &Path) -> std::io::Result<Option<DataDirInstanceGuard>> {
    wait_for_instance_guard_with(db_path, WAIT_MAX_ATTEMPTS, WAIT_DELAY)
}

fn wait_for_instance_guard_with(
    db_path: &Path,
    max_attempts: u32,
    delay: Duration,
) -> std::io::Result<Option<DataDirInstanceGuard>> {
    for attempt in 0..max_attempts {
        match DataDirInstanceGuard::try_acquire(db_path)? {
            Some(guard) => return Ok(Some(guard)),
            None => {
                if attempt + 1 < max_attempts {
                    std::thread::sleep(delay);
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn yields_after_bounded_wait_when_peer_holds_the_guard() {
        let dir = std::env::temp_dir().join(format!("aionui-wait-guard-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aionui-backend.db");

        // A peer already owns the data dir.
        let _held = DataDirInstanceGuard::try_acquire(&db_path)
            .expect("try_acquire should not error")
            .expect("first acquisition should win");

        // Small, fast bound so the test stays quick while still exercising the
        // "poll then yield" path.
        let attempts = 3;
        let delay = Duration::from_millis(10);
        let started = Instant::now();
        let result = wait_for_instance_guard_with(&db_path, attempts, delay).expect("wait should not error");
        let elapsed = started.elapsed();

        assert!(result.is_none(), "must yield while the peer holds the guard");
        // It must not block indefinitely, and it must have slept between attempts.
        assert!(elapsed >= delay, "should have waited at least one delay");
        assert!(elapsed < Duration::from_secs(2), "should not block indefinitely");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acquires_immediately_when_data_dir_is_free() {
        let dir = std::env::temp_dir().join(format!("aionui-wait-guard-free-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("aionui-backend.db");

        let guard = wait_for_instance_guard_with(&db_path, WAIT_MAX_ATTEMPTS, WAIT_DELAY)
            .expect("wait should not error")
            .expect("acquisition should win on a free data dir");
        drop(guard);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
